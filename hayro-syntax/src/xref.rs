//! Reading and querying the xref table of a PDF file.
//!
//! # The table a document is read through
//!
//! 1. The table is read from the cross-reference section the last `startxref` of the
//!    file leads to and the sections its `/Prev` and `/XRefStm` entries lead to, in
//!    turn: an entry in use of a later section replaces an earlier section's for the
//!    same object, the stream a table's `/XRefStm` names counting as before that
//!    table. A free entry lists nothing and replaces nothing.
//! 2. Before any object of the document is read, the table is checked. It is kept
//!    where every section could be read, its trailer's /Root is a dictionary, and
//!    every entry in use leads to its object:
//!    - an entry in use at offset 0 is taken as free;
//!    - an entry into the file leads to its object where, at its offset exactly,
//!      not after white space or a comment, a header `N G obj` names the entry's
//!      number and generation and ends before the next offset any entry names;
//!    - an entry into an object stream leads to its object where the table lists
//!      the stream's number as an object of the file, whatever that object holds.
//! 3. The check reads headers only. An object after its header that cannot be
//!    read, or does not end before the next offset any entry names, is no reason
//!    to rebuild the table: it is lost when it is read ([`XRef::watch`]). Neither
//!    the trailer's /Size, smaller or larger than the entries or missing, nor an
//!    entry a later section replaces plays a part.
//! 4. A table that is not kept is rebuilt from a scan of the file, when the file
//!    opens. A table never changes once the file is open: every value read from
//!    the document is read from one table.
//!
//! # A rebuilt table
//!
//! The checked table is the old table; the scan finds headers, each with an object
//! after it that can be read or not, and object streams.
//!
//! 5. The rebuilt table holds each object whose header the scan finds with an
//!    object after it that can be read; of two such definitions of one object, the
//!    later in the file.
//! 6. Of an object's definitions in the object streams the scan finds, the table
//!    takes: where the old table lists the object in an object stream that holds
//!    it, the last definition in that stream; else, where rule 5 gives it a
//!    definition outside object streams, that one; else the last definition in an
//!    object stream, in the order of the file. Where no table could be read, the
//!    old table lists nothing. A definition in an object stream is read from the
//!    stream the scan found it in, whatever definition of the stream's number the
//!    table holds; the stream the old table lists is the definition of its number
//!    the old entry leads to.
//! 7. An old entry that leads into a header the scan found, after its start and
//!    before the object after it, leads to that header.
//! 8. An old entry that leads to a header with an object after it that can be read,
//!    to its end however far that is, is kept where the table holds no object of
//!    the entry's number and generation: the entry's object is the one at that
//!    header, read under the entry's identifier and decrypted as the header names.
//!    Where the header names another object, which the old table does not also list
//!    at that header, the table gives that object the last of its definitions the
//!    scan found elsewhere, or, with none, does not list it.
//! 9. An object of which the table holds no definition is listed, but lost when it
//!    is read ([`XRef::has_entry`]), where the scan finds its header with no object
//!    that can be read after it, or an old entry of its identifier leads to such a
//!    header or to one that the next offset any old entry names cuts.
//! 10. An old entry that leads to no header adds nothing.
//! 11. The trailer is the last dictionary the scan finds with a /Root that is a
//!     dictionary holding /Pages, read through the rebuilt table; where there is
//!     none, the catalog is the object of the last header before the last dictionary
//!     typed /Catalog. A document whose trailer has /Encrypt is rebuilt again with its
//!     decryptor, which finds the objects of encrypted object streams.
//!
//! Checking a table and rebuilding it read each part of the file a bounded number of
//! times, however many entries lead to one place or into objects that hold others.
//!
//! `PyMuPDF` 1.28.2 differs on purpose (tests `xref_check.rs`): it repairs a table
//! on demand, when a read finds another header than the entry's, so that values read
//! before the repair can differ from those read after; it reads at an offset past
//! white space and comments; in a repair it takes the last definition of an object,
//! in an object stream or not (rule 6), and reads an entry that leads to another
//! object's header as null (rule 8).

use crate::crypto::reader::DecryptedReader;
use crate::crypto::{DecryptionError, DecryptionTarget, Decryptor, get};
use crate::data::{Data, Decoded};
use crate::hash;
use crate::lost::Lost;
use crate::metadata::Metadata;
use crate::object::Name;
use crate::object::ObjectIdentifier;
use crate::object::Stream;
use crate::object::dict::keys::{
    AUTHOR, CREATION_DATE, CREATOR, ENCRYPT, FIRST, ID, INDEX, INFO, KEYWORDS, LENGTH, MOD_DATE, N,
    OCPROPERTIES, PAGES, PREV, PRODUCER, ROOT, SIZE, SUBJECT, TITLE, TYPE, VERSION, W, XREF_STM,
};
use crate::object::indirect::IndirectObject;
use crate::object::parsed::Top;
use crate::object::{Array, MaybeRef};
use crate::object::{DateTime, Dict};
use crate::object::{Object, ObjectLike, Written};
use crate::pdf::PdfVersion;
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt};
use crate::sync::{Arc, Mutex, MutexExt, OnceLock};
use crate::trivia::{is_eol_character, is_regular_character, is_white_space_character};
use crate::util::findr_needle;
use crate::{PdfData, object};
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::max;
use core::iter;
use core::num::NonZeroUsize;
use core::ops::Deref;

mod body;
mod scan;

pub(crate) use body::Place;
use body::{Header, Slot, SlotReader, Source, Taken, Unread};
use scan::Scan;

pub(crate) const XREF_ENTRY_LEN: usize = 20;

/// Identifies a loaded document among every document loaded in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocumentId(u64);

impl DocumentId {
    /// A new identifier; `None` on a target without 64-bit atomics to count with.
    fn new() -> Option<Self> {
        #[cfg(target_has_atomic = "64")]
        {
            use core::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            Some(Self(NEXT.fetch_add(1, Ordering::Relaxed)))
        }
        #[cfg(not(target_has_atomic = "64"))]
        None
    }
}

/// Identifies what is read from bytes of a document, for a cache of what is parsed or
/// copied from them: the bytes, by where they are, in the file or in the decoded data
/// of an object stream; the document; and, where it is encrypted, the object whose key
/// decrypts their strings and streams ([`ReaderContext::decryption`]).
///
/// That object is the one whose header the bytes are under, so in a document whose
/// objects do not overlap the bytes alone decide it, and entries of a damaged table
/// that lead to one object find one key for what they read there. Where a damaged
/// file nests one object's header inside another's dictionary, the bytes they share
/// decrypt as each of them, and their keys differ.
///
/// The crypt filter is the document's string filter, or for a stream the one its
/// dictionary, in these bytes, names. A key is only made from bytes read in a context
/// ([`Dict::content_key`](crate::object::Dict::content_key),
/// [`Array::content_key`](crate::object::Array::content_key)), so none can leave the
/// decryption out.
///
/// Two keys are equal where they name the same bytes in the same place: the key
/// borrows them, so no other bytes can take their place while it is held. For a cache
/// that outlives the bytes, [`Self::value`] names them by value.
#[derive(Debug, Clone, Copy)]
pub struct ContentKey<'a> {
    bytes: &'a [u8],
    document: Option<DocumentId>,
    decryption: Option<ObjectIdentifier>,
}

impl<'a> ContentKey<'a> {
    /// The key of `bytes`, read in `ctx`.
    pub(crate) fn new(bytes: &'a [u8], ctx: &ReaderContext<'a>) -> Self {
        Self {
            bytes,
            document: ctx.xref().document_id(),
            decryption: ctx.decryption(),
        }
    }

    /// What the key names, the bytes by value: for a cache that may outlive them,
    /// where other bytes may take their place.
    pub fn value(&self) -> impl core::hash::Hash + Eq + 'a {
        (self.bytes, self.document, self.decryption)
    }

    /// The bytes by where they are, with the document and the decryption, where the
    /// document has an identifier ([`StreamId`]).
    pub(crate) fn stream_id(&self) -> Option<StreamId> {
        let (at, document, decryption) = self.parts();
        Some(StreamId {
            document: document?,
            at,
            decryption,
        })
    }

    /// What equality, order, and hashing compare: the bytes by where they are.
    fn parts(&self) -> ((usize, usize), Option<DocumentId>, Option<ObjectIdentifier>) {
        (
            (self.bytes.as_ptr() as usize, self.bytes.len()),
            self.document,
            self.decryption,
        )
    }
}

impl PartialEq for ContentKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.parts() == other.parts()
    }
}

impl Eq for ContentKey<'_> {}

impl PartialOrd for ContentKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ContentKey<'_> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.parts().cmp(&other.parts())
    }
}

impl core::hash::Hash for ContentKey<'_> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.parts().hash(state);
    }
}

/// Identifies a stream object among the streams of every document loaded in this
/// process ([`Stream::identity`](crate::object::Stream::identity)): its document,
/// where its dictionary lies in the data the document holds while it is loaded, and
/// the object whose key decrypts it. Every entry of the table that leads to one
/// body finds one identity, and two bodies have two; a document loaded later has
/// another [`DocumentId`], so no identity a cache holds names another's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId {
    document: DocumentId,
    at: (usize, usize),
    decryption: Option<ObjectIdentifier>,
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum XRefError {
    Unknown,
    Encryption(DecryptionError),
}

/// The table of the PDF: the one its last `startxref` leads to, or, where that cannot
/// be read or has an entry that does not lead to its object, one rebuilt by a scan
/// of the file.
pub(crate) fn open(data: PdfData, password: &[u8]) -> Result<XRef, XRefError> {
    let mut xref_map = hash::Map::default();
    let trailer = find_last_xref_pos(data.as_ref())
        .and_then(|pos| populate_xref_impl(data.as_ref(), pos, &mut xref_map));
    let Some(trailer) = trailer else {
        return fallback(data, password, Old::default()).ok_or(XRefError::Unknown);
    };

    match check(&mut xref_map, data.as_ref()) {
        Ok(layout) => match XRef::new(
            XRef::table(
                data.clone(),
                Rebuilt::sound(layout),
                Arc::new(xref_map),
                Arc::new(Decryptor::None),
            ),
            XRefInput::TrailerDictData(trailer),
            password,
        ) {
            Err(XRefError::Unknown) => {
                fallback(data, password, Old::default()).ok_or(XRefError::Unknown)
            }
            xref => xref,
        },
        Err(old) => fallback(data, password, old).ok_or(XRefError::Unknown),
    }
}

/// The old table: what checking a table that is not kept finds of its entries, for
/// the rebuild ([`check`], [`old_offsets`], [`places`], [`merge`]). `Old::default()`
/// lists nothing: the old table where no table could be read, or where reading the
/// trailer of a table that was kept fails ([`open`]).
#[derive(Debug, Default)]
struct Old {
    /// Each entry that leads to a header with an object after it that can be read:
    /// its identifier, its offset, and the identifier the header names; in the order
    /// of the offsets once [`old_offsets`] has read on the entries of `unended`.
    led: Vec<(ObjectIdentifier, usize, ObjectIdentifier)>,
    /// Each entry that leads to a header, or to the start of one its slot cuts, with
    /// no object after it that ends in the slot: its identifier, and, where a header
    /// ends in the slot and the object after it starts in the slot, the entry's
    /// offset, where the object starts, and the identifier the header names.
    /// [`old_offsets`] reads these on, and empties it.
    unended: Vec<(ObjectIdentifier, Option<Unended>)>,
    /// Each entry into an object stream, and the stream's number.
    in_streams: hash::Map<ObjectIdentifier, u32>,
}

/// An entry of the old table that leads to a header that ends in its slot, after which
/// an object starts in the slot that does not end there ([`Old::unended`]): the
/// entry's offset, where the object starts, and the identifier the header names.
type Unended = (usize, usize, ObjectIdentifier);

/// What the slot of an offset of a table's entries starts with ([`check`]).
#[derive(Debug, Clone, Copy)]
enum Seen {
    /// A header that ends in the slot, naming this object, and where the object
    /// after it starts, where that is in the slot: so that, where the table is not
    /// kept, the object is read without reading the header again.
    Header(ObjectIdentifier, Option<usize>),
    /// The start of a header the slot's end cuts.
    Cut,
    /// No header, or the offset is past the end of the file.
    Nothing,
}

#[cfg(test)]
std::thread_local! {
    /// The work the scans of files whose tables are rebuilt have done on this thread
    /// besides lexing, which readers count ([`crate::work::EXAMINED`]): the bytes of
    /// white space and strings read, and the blocks, tree nodes and outcomes looked at
    /// (`scan::Scan`). The tests bound both to show that opening a file reads each
    /// part of it a bounded number of times.
    pub(crate) static OPEN_BYTES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[inline]
fn opened(_bytes: usize) {
    #[cfg(test)]
    OPEN_BYTES.set(OPEN_BYTES.get() + _bytes);
}

/// Checks the table `xref_map` of the file `data` before any object of the document
/// is read (rules 2 and 3 of the module doc), so that every value read from the
/// document is read from one table. Returns the layout of a table that is kept;
/// else the old table, for the rebuild (rules 4 to 10).
///
/// - Each entry in use at offset 0 is removed from `xref_map`, kept or not.
/// - The table is kept where every entry in use leads to its object, as rule 2
///   states: an entry into the file where a header naming its number and generation
///   is at its offset exactly and ends in its slot, before the next offset any entry
///   names ([`body::SlotReader::header`], [`Place`]); an entry into an object stream
///   where `xref_map` lists the stream's number as an object of the file.
/// - The layout of a kept table is over the offsets of its entries into the file.
///   Keeping a table reads headers only, and no body (rule 3; test
///   `opening_a_file_lexes_no_object_it_does_not_read`).
/// - Where the table is not kept, `Old` carries for the later steps, with each entry
///   its identifier and offset: the entries that lead to a header with an object
///   after it that ends in the slot, read as reading the entry's object reads it
///   ([`body::SlotReader::object`]), and the identifier the header names (rules 7
///   and 8); the entries that lead to a header, or to the start of one the slot cuts,
///   with no such object, and where the object after a header that ends in the slot
///   starts, where that is in the slot ([`old_offsets`], rules 8 and 9); and the
///   entries into object streams, with the stream's number (rule 6). An entry that
///   leads to no header adds nothing (rule 10).
/// - Each header, and each body where the table is not kept, is read once in its
///   slot however many entries lead to it (test
///   `opening_a_file_reads_it_in_linear_time`).
fn check(xref_map: &mut XrefMap, data: &[u8]) -> Result<Layout, Old> {
    // Each entry into the file, by offset, the order of entries at one offset playing
    // no part; and the stream of each entry into an object stream.
    let mut entries: Vec<(usize, ObjectIdentifier)> = Vec::with_capacity(xref_map.len());
    let mut streams = vec![];
    // A table read from the file has no entry a rebuild makes.
    let mut rebuilt = false;
    xref_map.retain(|id, entry| match *entry {
        EntryType::Normal(0) => false,
        EntryType::Normal(at) => {
            entries.push((at, *id));
            true
        }
        EntryType::ObjStream(stream, _) => {
            streams.push(stream);
            true
        }
        EntryType::Found(..) => {
            rebuilt = true;
            true
        }
    });
    let mut kept = !rebuilt
        && streams.iter().all(|stream| {
            i32::try_from(*stream).is_ok_and(|stream| {
                matches!(
                    xref_map.get(&ObjectIdentifier::new(stream, 0)),
                    Some(EntryType::Normal(_))
                )
            })
        });
    by_offset(&mut entries);
    let slot_end = |next: usize| {
        entries
            .get(next)
            .map_or(data.len(), |(at, _)| *at)
            .min(data.len())
    };
    // The entries at each offset, a run, read once for them all.
    let runs = || entries.chunk_by(|(at, _), (other, _)| at == other);
    // For each run, in order, where the object after the header its slot starts with
    // starts, where the header names each entry of the run and the object starts in
    // the slot; else `None`, and what the slot starts with is next in `odd`. Kept for
    // a table that is not kept, so that its headers are read once.
    let mut objects: Vec<Option<NonZeroUsize>> = Vec::with_capacity(entries.len());
    let mut odd: Vec<Seen> = vec![];
    let mut start = 0;
    for run in runs() {
        let (at, end) = (run[0].0, start + run.len());
        let seen = match SlotReader::new(data, at, Slot::to(slot_end(end))) {
            Some(mut r) => match r.header() {
                Header::Read(header) => Seen::Header(header, r.object_start()),
                Header::Cut => Seen::Cut,
                Header::Absent => Seen::Nothing,
            },
            None => Seen::Nothing,
        };
        let named = matches!(seen, Seen::Header(header, _)
            if run.iter().all(|(_, id)| *id == header));
        kept &= named;
        let object = match seen {
            Seen::Header(_, Some(object)) if named => NonZeroUsize::new(object),
            _ => None,
        };
        if object.is_none() {
            odd.push(seen);
        }
        objects.push(object);
        start = end;
    }
    if kept {
        // Each run's offset, collected straight into the layout.
        let mut runs = runs();
        return Ok(Layout {
            starts: (0..objects.len())
                .map(|_| runs.next().map_or(0, |run| run[0].0))
                .collect(),
            ends: Ends::default(),
        });
    }

    let mut old = Old::default();
    let mut odd = odd.into_iter();
    let mut start = 0;
    for (run, object) in runs().zip(objects) {
        let (at, end) = (run[0].0, start + run.len());
        let ids = run.iter().map(|(_, id)| *id);
        let seen = match object {
            Some(object) => Seen::Header(run[0].1, Some(object.get())),
            None => odd.next().expect("each run with no object is odd"),
        };
        match seen {
            Seen::Header(header, object) => {
                // The object after the header, read as reading the entry's object
                // reads it, a dictionary skipped rather than parsed.
                let slot = Slot::to(slot_end(end));
                let ctx = ReaderContext::dummy();
                let ends = object
                    .and_then(|object| SlotReader::new(data, object, slot))
                    .is_some_and(|r| {
                        let (read, looked) = r.object(
                            |r| r.skip::<Dict<'_>>(false),
                            |r| r.read_with_context::<Object<'_>>(&ctx),
                        );
                        crate::work::read_bodies(looked);
                        read.is_some()
                    });
                if ends {
                    old.led.extend(ids.map(|id| (id, at, header)));
                } else {
                    let unended = object.map(|object| (at, object, header));
                    old.unended.extend(ids.map(|id| (id, unended)));
                }
            }
            Seen::Cut => old.unended.extend(ids.map(|id| (id, None))),
            Seen::Nothing => {}
        }
        start = end;
    }
    old.in_streams = xref_map
        .iter()
        .filter_map(|(id, entry)| match entry {
            EntryType::ObjStream(stream, _) => Some((*id, *stream)),
            EntryType::Normal(_) | EntryType::Found(..) => None,
        })
        .collect();
    Err(old)
}

/// Sorts `entries` by their offsets, in time linear in their number for offsets in a
/// file of any size: a table lists its entries in no set order of offsets, and a
/// file opens with its table checked in this order ([`check`]).
fn by_offset(entries: &mut Vec<(usize, ObjectIdentifier)>) {
    const BITS: u32 = 11;
    const DIGITS: usize = 1 << BITS;
    // A pass counts in as many places as there are digits: fewer entries are compared.
    if entries.len() < DIGITS {
        entries.sort_unstable_by_key(|(at, _)| *at);
        return;
    }
    let last = entries.iter().map(|(at, _)| *at).max().unwrap_or(0);
    let mut sorted = vec![(0, ObjectIdentifier::new(0, 0)); entries.len()];
    let mut shift = 0;
    while shift < usize::BITS && last >> shift != 0 {
        let digit = |at: usize| (at >> shift) & (DIGITS - 1);
        let mut next = [0; DIGITS];
        for (at, _) in entries.iter() {
            next[digit(*at)] += 1;
        }
        let mut sum = 0;
        for count in &mut next {
            (*count, sum) = (sum, sum + *count);
        }
        for entry in entries.iter() {
            let to = &mut next[digit(entry.0)];
            sorted[*to] = *entry;
            *to += 1;
        }
        core::mem::swap(entries, &mut sorted);
        shift += BITS;
    }
}

// Rebuilds the table and finds the trailer from a scan of the file and the entries of
// the old table, `old` (rules 5 to 11 of the module doc).
fn fallback(data: PdfData, password: &[u8], old: Old) -> Option<XRef> {
    warn!("xref table was invalid, trying to manually build xref table");
    let (table, xref_input) = fallback_xref_map(&data, password, old);

    if let Some(xref_input) = xref_input {
        warn!("rebuild xref table with {} entries", table.len());

        XRef::new(table, xref_input, password).ok()
    } else {
        warn!("couldn't find trailer dictionary, failed to rebuild xref table");

        None
    }
}

/// The objects of a rebuilt table that cannot be read: a header names them, or an
/// entry of the old table leads to a header, but no object can be read after it, and
/// the rebuilt table has none of their number. A reference to one is no `null`, as a
/// reference to an object the file does not hold is: what refers to it lost it.
type Unreadable = BTreeSet<ObjectIdentifier>;

/// The identifier the header at each place of the file an entry of a rebuilt table
/// leads to names, where that is not the entry's own: where an entry of the old table
/// is kept for an object headed as another. Found once, where the table is rebuilt,
/// for every entry leading there; by place, so that what is read there, under any
/// identifier, is decrypted as its header names ([`XRef::entered`]).
type Headers = hash::Map<usize, ObjectIdentifier>;

/// The object streams a rebuild found, each its header's object and where the header
/// is, in the order of the file: an entry of a rebuilt table into an object stream
/// names one of these ([`EntryType::Found`]), not a number the table resolves.
type Found = Vec<(ObjectIdentifier, usize)>;

/// What a rebuild finds besides the entries of the table, of which a table read from
/// the file has nothing: the objects it lists that cannot be read, the places whose
/// headers name other objects than the entries leading there, and the object streams
/// it found.
#[derive(Debug, Clone, Default)]
struct Repairs {
    unreadable: Unreadable,
    headers: Headers,
    streams: Found,
}

/// What reading a table finds besides its entries: where the objects of the file
/// are, which their slots end at, and what a rebuild finds besides.
#[derive(Debug, Clone)]
struct Rebuilt {
    layout: Layout,
    repairs: Repairs,
}

impl Rebuilt {
    /// What a table read from the file finds: its objects are where `layout` puts
    /// them, at its entries.
    fn sound(layout: Layout) -> Self {
        Self {
            layout,
            repairs: Repairs::default(),
        }
    }
}

fn fallback_xref_map<'a>(
    data: &'a PdfData,
    password: &[u8],
    mut old: Old,
) -> (XRef, Option<XRefInput<'a>>) {
    let mut scan = Scan::new(data.as_ref());
    let (offsets, unread) = old_offsets(&mut old, &mut scan);
    let walk = walk(data, &mut scan, &offsets);
    fallback_xref_map_inner(data, &walk, None, password, &old, &unread)
}

/// Before the walk: reads on through `scan` ([`Scan::object`]) the object of each
/// entry of `old` that leads to a header that ends in its slot but to no object that
/// ends there, from where the object starts, where that is in the slot, as reading the
/// entry's object reads it (rule 8: an object that can be read to its end however far
/// that is); an entry whose header the slot cuts, or whose object does not start in
/// the slot, is not read on. Each part of the file is read a bounded number of times
/// however many entries lead into one string or container.
///
/// Returns where each entry of `old` that leads to an object leads, those read on
/// included, in rising order, which the walk takes for places ([`walk`]); and the
/// identifiers of the entries that lead to no object that can be read (rule 9). `old`
/// then holds the entries read on as entries that lead to an object, for [`places`]
/// and [`merge`].
fn old_offsets(old: &mut Old, scan: &mut Scan<'_>) -> (Vec<usize>, Vec<ObjectIdentifier>) {
    let mut unended = core::mem::take(&mut old.unended);
    // In rising order, as the scan is asked, each place once.
    unended.sort_unstable_by_key(|(_, object)| object.map(|(_, start, _)| start));
    let mut unread = vec![];
    for (id, object) in unended {
        match object {
            Some((at, start, header)) if scan.object(start).is_some() => {
                old.led.push((id, at, header));
            }
            _ => unread.push(id),
        }
    }
    old.led.sort_unstable_by_key(|(id, at, _)| (*at, *id));
    let mut offsets: Vec<usize> = old.led.iter().map(|(_, at, _)| *at).collect();
    offsets.dedup();
    (offsets, unread)
}

/// What a scan of the file finds, which does not depend on how the document is
/// decrypted: the objects, where their bodies start, the headers with no object after
/// them, the dictionaries that may be trailers, the last header before a catalog, the
/// object streams, and where `endstream` is, if the walk looked for it. Found once,
/// also for a document whose table is rebuilt again with its decryptor.
struct Walk<'a> {
    xref_map: XrefMap,
    found: Vec<(ObjectIdentifier, usize)>,
    bodies: Vec<usize>,
    broken: Vec<ObjectIdentifier>,
    trailer_dicts: Vec<Dict<'a>>,
    root_ref: Option<ObjectIdentifier>,
    object_streams: Vec<(ObjectIdentifier, usize)>,
    ends: Ends,
}

/// Walks the file for [`fallback_xref_map_inner`], reading each part of it a bounded
/// number of times with `scan`, where the entries of the old table that lead to an
/// object lead, `listed`, in rising order ([`old_offsets`]). The keys it reads of a
/// dictionary are names, which encryption leaves as they are.
fn walk<'a>(data: &'a PdfData, scan: &mut Scan<'_>, listed: &[usize]) -> Walk<'a> {
    let mut dummy_ctx = ReaderContext::dummy();
    let mut xref_map = hash::Map::default();
    // Each object found, in the order of the file, and where its body starts.
    let mut found = vec![];
    let mut bodies = vec![];
    // Each header found with no object that can be read after it, and where it is.
    let mut broken = vec![];
    // Each dictionary that may be a trailer, and where it is.
    let mut trailer_dicts = vec![];
    // Where each catalog is.
    let mut catalogs = vec![];
    // Where the walk went on just after an `endstream` ([`Walked::resumed`]).
    let mut resumed = vec![];

    let mut r = Reader::new(data.as_ref());

    // The object streams found: the object, and where its header is.
    let mut object_streams = vec![];
    // The last header found with an object after it, where it is, and where its
    // object starts.
    let mut last_header = None;
    let mut streams = Streams::default();

    loop {
        let cur_pos = r.offset();

        if !streams.unsure.is_empty()
            && data.as_ref()[..cur_pos].ends_with(b"endstream")
            && last_header.is_none_or(|(_, _, body)| body < cur_pos)
        {
            resumed.push(cur_pos);
        }

        // First try to check if we have an object identifier.
        if let Some(header) = rebuilt_header(&mut r, scan) {
            match header {
                Some((obj_id, Some(body))) => {
                    xref_map.insert(obj_id, EntryType::Normal(cur_pos));
                    found.push((obj_id, cur_pos));
                    bodies.push(body);
                    last_header = Some((obj_id, cur_pos, body));
                    dummy_ctx.set_obj_number(obj_id);
                }
                Some((obj_id, None)) => broken.push((obj_id, cur_pos)),
                // There must be a white space before the next object number.
                None => r.forward_while(|b| !is_white_space_character(b)),
            }
        } else {
            // Then, try to check whether we have a dictionary, in particular a trailer
            // dictionary: the scan finds where it ends and whether it has the keys
            // that make one worth reading.
            if r.peek_bytes(2).is_some_and(|b| b == b"<<")
                && let Some(found) = scan.object(cur_pos)
            {
                r.jump(found.end);
                // The dictionary of a stream that is a header's object, whose data the
                // walk skips ([`Streams::after`]).
                let body = last_header.is_some_and(|(_, _, body)| body == cur_pos);
                let stream = body
                    .then(|| stream_data_start(data.as_ref(), found.end))
                    .flatten();
                if found.root || found.kind || stream.is_some() {
                    let mut dict_reader = Reader::new(&data.as_ref()[cur_pos..found.end]);
                    if let Some(dict) = dict_reader.read_with_context::<Dict<'_>>(&dummy_ctx) {
                        if let Some(start) = stream
                            && let Some(after) = streams.after(data.as_ref(), start, &dict)
                        {
                            r.jump(after);
                        }
                        if found.root && dict.contains_key(ROOT) {
                            trailer_dicts.push((cur_pos, dict.clone()));
                        }

                        if dict
                            .get::<Name<'_>>(TYPE)
                            .is_some_and(|n| n.as_str() == "Catalog")
                        {
                            catalogs.push(cur_pos);
                        }

                        // An object stream is the body of its header's object, read
                        // once the scan has found every place, which its data may
                        // not reach past ([`XRef::object_stream_at`]).
                        if dict.get::<Name<'_>>(TYPE).as_deref() == Some(b"ObjStm")
                            && let Some((id, at, _)) = last_header
                            && body
                        {
                            object_streams.push((id, at));
                        }
                    }
                }
            } else if r.peek_byte() == Some(b'%') {
                // A comment holds no object, and is read once.
                r.forward_while(|b| !is_eol_character(b));
            } else {
                // We can skip everything until the next white space character,
                // as there cannot possibly be any new dictionary/object identifier
                // until then; and a run of white space, where none starts.
                let old_pos = r.offset;
                r.forward_while(|b| !is_white_space_character(b));
                if r.offset == old_pos {
                    r.forward_while(is_white_space_character);
                }
            }
        }

        if r.at_end() {
            break;
        }
    }

    // What the walk read of data a walk skipping them would not have, taken out: as
    // that walk would have gone on from where this one did, it would have found the
    // same after them ([`unread_data`]).
    let mut skipped = vec![];
    if !streams.unsure.is_empty() {
        skipped = unread_data(
            data.as_ref(),
            &Walked {
                found: &found,
                bodies: &bodies,
                last: &xref_map,
                unsure: &streams.unsure,
                resumed: &resumed,
                listed,
                ends: &streams.ends,
            },
        );
    }
    if !skipped.is_empty() {
        let read = |at: &usize| {
            let next = skipped.partition_point(|skipped| skipped.start <= *at);
            !next.checked_sub(1).is_some_and(|i| skipped[i].holds(*at))
        };
        let kept: Vec<usize> = (0..found.len()).filter(|i| read(&found[*i].1)).collect();
        let table: XrefMap = kept
            .iter()
            .map(|i| (found[*i].0, EntryType::Normal(found[*i].1)))
            .collect();
        // Checked, not assumed: the table holds each header a /Length was read from.
        // Where one it does not hold, the walk keeps all it read.
        let held = skipped.iter().all(|skipped| {
            table.get(&skipped.length.0) == Some(&EntryType::Normal(skipped.length.1))
        });
        debug_assert!(held, "a /Length read from a header the table does not hold");
        if held {
            bodies = kept.iter().map(|i| bodies[*i]).collect();
            found = kept.iter().map(|i| found[*i]).collect();
            xref_map = table;
            broken.retain(|(_, at)| read(at));
            trailer_dicts.retain(|(at, _)| read(at));
            catalogs.retain(read);
            object_streams.retain(|(_, at)| read(at));
        }
    }
    // The object of the last header before the last catalog.
    let root_ref = catalogs.last().and_then(|catalog| {
        let next = found.partition_point(|(_, at)| at < catalog);
        Some(found[next.checked_sub(1)?].0)
    });

    Walk {
        xref_map,
        found,
        bodies,
        broken: broken.into_iter().map(|(id, _)| id).collect(),
        trailer_dicts: trailer_dicts.into_iter().map(|(_, dict)| dict).collect(),
        root_ref,
        object_streams,
        ends: streams.ends,
    }
}

/// Where the data of a stream start, whose dictionary ends at `end` of `data`: after
/// the keyword `stream`, after white space and comments, and the end of line after
/// it, if there is one; `None` where no `stream` follows.
fn stream_data_start(data: &[u8], end: usize) -> Option<usize> {
    let mut r = Reader::new(data);
    r.jump(end);
    r.skip_white_spaces_and_comments();
    r.forward_tag(b"stream")?;
    let _ = r
        .forward_tag(b"\n")
        .or_else(|| r.forward_tag(b"\r\n"))
        .or_else(|| r.forward_tag(b"\r"));
    Some(r.offset())
}

/// What the walk of a rebuild finds of the streams that are headers' objects, to go on
/// after their data, which it does not read: the first `endstream` and `stream` at or
/// after an offset, and the streams whose data it reads though an indirect /Length
/// may end them.
#[derive(Default)]
struct Streams {
    /// Where each `endstream` of the file is, found the first time one is looked for
    /// past a long run of white space, as reading a stream finds them; the rebuilt
    /// table's [`Layout`] keeps them.
    ends: Ends,
    keywords: NextKeyword,
    /// Where the data of each such stream start, and the object its /Length refers
    /// to, in the order of the file ([`unread_data`]).
    unsure: Vec<(usize, ObjectIdentifier)>,
}

impl Streams {
    /// Where the walk goes on after the data of a stream that start at `start` of
    /// `data`, `dict` its dictionary: after the `endstream` a /Length that is a
    /// number leads to past white space, as reading the stream finds its data
    /// ([`endstream_past_white`]); else after the first `endstream` from `start` on,
    /// as reading finds the data of a stream whose /Length does not lead to one,
    /// unless another stream starts before it. `None` where neither ends the data:
    /// the walk reads them, and a /Length that is a reference is kept, to be read
    /// once the walk has found the object it refers to ([`unread_data`]).
    ///
    /// So a header or dictionary in a stream's data, such as an object of a PDF
    /// attached uncompressed, is not taken for one of the file's, and the walk does
    /// not read the data, most of a file.
    // Not inlined: in the walk's loop it took the registers of the loops that read
    // white space and tokens, which then reload them for each byte.
    #[inline(never)]
    fn after(&mut self, data: &[u8], start: usize, dict: &Dict<'_>) -> Option<usize> {
        let reference = dict.get_ref(LENGTH);
        if reference.is_none()
            && let Some(at) = dict
                .get::<u32>(LENGTH)
                .and_then(|length| led(data, start, length, &self.ends))
        {
            return Some(at + b"endstream".len());
        }
        // As MuPDF repairs a table: PyMuPDF 1.28.2 finds no object whose header is in
        // a stream's data up to where /Length leads, if it is a number leading to
        // `endstream`, else up to the first `endstream`. That one is another stream's
        // where a `stream` keyword comes before it, such as an attached PDF's, or the
        // next stream's after data whose own `endstream` is missing or misspelled:
        // the objects up to it are the file's, which PyMuPDF loses. The walk reads
        // such data, as it does where no `endstream` follows, keeping those objects.
        match self.keywords.at_or_after(data, start)? {
            Keyword::End(first) => Some(first + b"endstream".len()),
            Keyword::Start => {
                if let Some(reference) = reference {
                    self.unsure.push((start, reference.into()));
                }
                None
            }
        }
    }
}

/// Where the `endstream` is that a /Length `length` leads to, of a stream whose data
/// start at `start` of `data` ([`endstream_past_white`]).
fn led(data: &[u8], start: usize, length: u32, ends: &Ends) -> Option<usize> {
    endstream_past_white(data, start.checked_add(length as usize)?, ends)
}

/// Where the `endstream` is that the data of a stream end before, past white space
/// only, where they end at `to` of `data`, as reading the stream finds it: past at
/// most [`NEAR`] bytes of white space read, else the first `endstream` after `to`
/// among where each is in the file, `ends`, found once.
fn endstream_past_white(data: &[u8], to: usize, ends: &Ends) -> Option<usize> {
    let near = data.get(to..)?;
    let white = near
        .iter()
        .take(NEAR)
        .take_while(|b| is_white_space_character(**b))
        .count();
    opened(white + 1);
    if white < NEAR {
        return near[white..]
            .starts_with(b"endstream")
            .then_some(to + white);
    }
    let (stop, trimmed) = first_endstream(ends, data, to)?;
    (trimmed <= to).then_some(stop)
}

/// The first `endstream` at or after `from` of the file, `data`, and where the data
/// before it end, without the white space before it, among where each is, `ends`,
/// found the first time one is looked for.
fn first_endstream(ends: &Ends, data: &[u8], from: usize) -> Option<(usize, usize)> {
    let ends = ends.found(|| {
        let ends = object::stream::searched_all(data, b"endstream")
            .map(|stop| {
                // The white space before it, up to the end of the one before at most,
                // which ends in none.
                (
                    stop,
                    object::stream::trim_trailing_ascii_whitespace(&data[..stop]),
                )
            })
            .collect();
        crate::work::read_bodies(data.len());
        ends
    });
    ends.get(ends.partition_point(|(at, _)| *at < from))
        .copied()
}

/// Data of a stream that a walk skipping them would not read ([`unread_data`]): from
/// `start` to `end`, the end of the `endstream` that the header at `length.1` of the
/// object `length.0`, the stream's /Length, leads to.
#[derive(Debug, Clone, Copy)]
struct Skipped {
    start: usize,
    end: usize,
    length: (ObjectIdentifier, usize),
}

impl Skipped {
    fn holds(&self, at: usize) -> bool {
        (self.start..self.end).contains(&at)
    }
}

/// What the walk of a rebuild records of where it went, to take out what it read of
/// the data of streams whose /Length, a reference, it did not know ([`unread_data`]).
struct Walked<'a> {
    /// Each header the walk put in the table, and where it is, in the order of the
    /// file; and where the object after each starts.
    found: &'a [(ObjectIdentifier, usize)],
    bodies: &'a [usize],
    /// The table the walk built: the last of `found` of each object.
    last: &'a XrefMap,
    /// [`Streams::unsure`].
    unsure: &'a [(usize, ObjectIdentifier)],
    /// Each place the walk went on from just after an `endstream`, after the first
    /// stream of `unsure`, with no header before it whose object starts after it.
    resumed: &'a [usize],
    /// Where the entries of the old table that lead to an object lead, in rising
    /// order: places, as the headers found are, where a body read ends ([`Layout`]).
    listed: &'a [usize],
    ends: &'a Ends,
}

/// The data of streams that the walk read but that a walk skipping them by their
/// /Length, a reference, would not have read: each from where the data start to the
/// end of the `endstream` that ends them. The table holds none of the objects the
/// walk found there.
///
/// The value of such a /Length is known only from the table, which the walk builds:
/// so the walk reads the data, as before it skipped any, and records each header it
/// finds, each such stream, and where it went on just after an `endstream`. Then,
/// for each stream in the order of the file that no data taken out hold, the object
/// is read from at most two headers, in this order: the last before the data, which
/// the walk's table held there, and the last of the file, the table's now. A header
/// is taken where:
///
/// - its value, read as the table reads the object, leads to an `endstream` past
///   white space, as a /Length that is a number does ([`led`]);
/// - the walk went on from just after that `endstream`: from there, a walk skipping
///   the data goes on as this one did, and finds what it found;
/// - with the headers in the data taken out, the table holds that header for the
///   object: the /Length is the one a reader of the rebuilt table reads;
/// - the data hold no header taken for an earlier stream's /Length, which taking
///   them out would take out of the table.
///
/// The header before the data is tried first: an attached PDF can hold, after its
/// first `endstream`, an object numbered alike whose value leads there.
///
/// Where no header is taken, the walk has read the data and takes the objects in
/// them, as before it skipped any: a PDF attached uncompressed whose /Length the
/// table does not hold, or holds as no number leading to `endstream`.
///
/// Each stream reads at most two headers' objects, each once, in its slot, which no
/// other's overlaps, and looks up the rest: the work is linear in the walk's, however
/// streams' data overlap and however many share a /Length.
fn unread_data(data: &[u8], walked: &Walked<'_>) -> Vec<Skipped> {
    let Walked {
        found,
        bodies,
        last,
        unsure,
        resumed,
        listed,
        ends,
    } = *walked;
    let ctx = ReaderContext::dummy();
    let index = |at: usize| found.partition_point(|(_, found)| *found < at);
    let mut values = hash::Map::default();
    // The object after the header `found[i]`, read as the table reads it, in its slot:
    // up to the next header found, or the next place an entry of the old table leads
    // to, which is past where the object starts.
    let mut value = |i: usize| -> Option<u32> {
        *values.entry(i).or_insert_with(|| {
            let at = found[i].1;
            let slot = found.get(i + 1).map_or(data.len(), |(_, next)| *next).min(
                listed
                    .get(listed.partition_point(|l| *l < bodies[i]))
                    .map_or(data.len(), |l| *l),
            );
            let place = Place {
                source: Source::File,
                at,
                decryption: None,
            };
            match body::Shape::read(data, Slot::to(slot), place, &ctx) {
                body::Shape::Value(top) => top.object(data, &ctx).cast::<u32>(),
                _ => None,
            }
        })
    };
    let mut skipped: Vec<Skipped> = vec![];
    // The last header of each object up to the data of the stream looked at, of those
    // not taken out: the table of the walk skipping the data taken out, there.
    let mut table: hash::Map<ObjectIdentifier, usize> = hash::Map::default();
    let mut next = 0;
    // The headers taken for a /Length after the data they end.
    let mut kept = BTreeSet::new();
    for &(start, id) in unsure {
        // Data in data taken out, which a walk skipping them does not reach.
        if skipped.last().is_some_and(|unread| start < unread.end) {
            continue;
        }
        while let Some(&(header, at)) = found.get(next)
            && at < start
        {
            if !skipped.last().is_some_and(|unread| unread.holds(at)) {
                table.insert(header, next);
            }
            next += 1;
        }
        let before = table.get(&id).copied();
        let last = match last.get(&id) {
            Some(EntryType::Normal(at)) if *at > start => Some(index(*at)),
            _ => before,
        };
        let Some(last) = last else { continue };
        for header in [before, Some(last).filter(|last| Some(*last) != before)]
            .into_iter()
            .flatten()
        {
            opened(1);
            let Some(end) = value(header)
                .and_then(|length| led(data, start, length, ends))
                .map(|at| at + b"endstream".len())
            else {
                continue;
            };
            let data_holds = |at: usize| (start..end).contains(&at);
            let table_holds = if header == last {
                !data_holds(found[last].1)
            } else {
                data_holds(found[last].1)
            };
            let rejoined = end >= data.len() || resumed.binary_search(&end).is_ok();
            if table_holds && rejoined && kept.range(start..end).next().is_none() {
                skipped.push(Skipped {
                    start,
                    end,
                    length: (id, found[header].1),
                });
                if found[header].1 > start {
                    kept.insert(found[header].1);
                }
                break;
            }
        }
    }
    skipped
}

/// The first keyword at or after an offset of a file that ends or starts the data of
/// a stream.
#[derive(Clone, Copy)]
enum Keyword {
    /// An `endstream`, where it is.
    End(usize),
    /// A `stream` that is a token, no regular character before or after it.
    Start,
}

/// The first [`Keyword`] at or after an offset of a file, for offsets asked in rising
/// order: each part of the file is searched once, as the one found is the first after
/// every offset up to it. Both hold `stream`, which is searched for.
#[derive(Default)]
struct NextKeyword {
    /// The offset last searched from, and where the `stream` of the keyword found from
    /// it is, and the keyword, if any.
    last: Option<(usize, Option<(usize, Keyword)>)>,
}

impl NextKeyword {
    fn at_or_after(&mut self, data: &[u8], at: usize) -> Option<Keyword> {
        match self.last {
            Some((from, Some((found, keyword)))) if from <= at && at <= found => {
                // An `endstream` that starts before `at` is not after it.
                if !matches!(keyword, Keyword::End(end) if end < at) {
                    return Some(keyword);
                }
            }
            Some((from, None)) if from <= at => return None,
            _ => {}
        }
        let regular = |i: usize| data.get(i).is_some_and(|b| is_regular_character(*b));
        let mut from = at;
        let found = loop {
            let Some(found) = data
                .get(from..)
                .and_then(|rest| object::stream::searched(rest, b"stream"))
            else {
                break None;
            };
            let found = from + found;
            if let Some(end) = found.checked_sub(b"end".len())
                && end >= at
                && data[end..].starts_with(b"end")
            {
                break Some((found, Keyword::End(end)));
            }
            if !(found.checked_sub(1).is_some_and(regular) || regular(found + b"stream".len())) {
                break Some((found, Keyword::Start));
            }
            from = found + 1;
        };
        self.last = Some((at, found));
        found.map(|(_, keyword)| keyword)
    }
}

/// The table a scan of the file rebuilds from its `walk`, where objects in object
/// streams are decrypted by `decryptor`, `None` until the trailer is known: a
/// document whose trailer has /Encrypt is rebuilt again with its decryptor, from the
/// same walk.
///
/// Every object the rebuild reads, an object stream it finds or a catalog a trailer
/// names, is read as the table it rebuilds reads it, through a table of what it has
/// found, by place ([`XRef::object_stream_at`], [`XRef::get_with`]): each body once,
/// in its slot, and each object stream's data decoded once, cut at the next place
/// holding a stream, however many objects the scan finds in it, entries name it or
/// trailers name a catalog in it. The places are every header the scan finds with an
/// object after it, and every one the old table leads to, the same for the rebuild's
/// reads and for the rebuilt table's ([`Layout`]): so the bodies read, and the data
/// of object streams decoded, are disjoint, whichever definitions of one number the
/// table keeps.
fn fallback_xref_map_inner<'a>(
    data: &'a PdfData,
    walk: &Walk<'a>,
    decryptor: Option<Arc<Decryptor>>,
    password: &[u8],
    old: &Old,
    unread: &[ObjectIdentifier],
) -> (XRef, Option<XRefInput<'a>>) {
    let Walk {
        found,
        bodies,
        broken,
        root_ref,
        ..
    } = walk;
    let mut xref_map = walk.xref_map.clone();
    let streams = walk.object_streams.clone();
    let trailer_dicts = walk.trailer_dicts.clone();
    let layout = Layout {
        ends: walk.ends.clone(),
        ..Layout::over(
            found
                .iter()
                .map(|(_, at)| *at)
                .chain(places(old, found, bodies))
                .collect(),
        )
    };
    let decrypted = decryptor
        .clone()
        .unwrap_or_else(|| Arc::new(Decryptor::None));
    // Each definition the scan found in an object stream: the object's number, the
    // stream, by its index in `streams`, and the index in it, in the order of the file.
    let mut held = vec![];
    if !streams.is_empty() {
        // The objects each object stream holds, read through a table of the objects
        // found, which a stream's /Length may be. The rebuilt table reads a stream
        // again where it reads an object in it: what a stream's dictionary refers to
        // may be another object there, as the table then holds the streams' objects.
        let probe = XRef::table(
            data.clone(),
            Rebuilt::sound(layout.clone()),
            Arc::new(xref_map),
            decrypted.clone(),
        );
        for (stream, &(header, at)) in streams.iter().enumerate() {
            if let Some(objects) = probe.object_stream_at(at, header) {
                held.extend(
                    objects
                        .offsets
                        .iter()
                        .enumerate()
                        .map(|(idx, (obj_num, _))| (*obj_num, stream, idx)),
                );
            }
        }
        xref_map = probe.into_map();
    }

    let (headers, unreadable) = merge(
        &mut xref_map,
        &held,
        &streams,
        found,
        bodies,
        broken,
        old,
        unread,
    );
    let rebuilt = Rebuilt {
        layout,
        repairs: Repairs {
            unreadable,
            headers,
            streams,
        },
    };

    // Try to choose the right trailer dict by doing basic validation: the last whose
    // /Root is a dictionary with /Pages, read as the rebuilt table reads it. The
    // table is the one the file is read through ([`XRef::new`]): what this reads,
    // the catalog, is read once.
    let has_pages = |dict: &Dict<'_>| -> bool { dict.contains_key(PAGES) };
    let table = XRef::table(data.clone(), rebuilt, Arc::new(xref_map), decrypted);
    let mut trailer_dict = None;
    for dict in trailer_dicts {
        let valid = match dict.get_raw::<Dict<'_>>(ROOT) {
            Some(MaybeRef::Ref(r)) => table
                .get::<Dict<'_>>(r.into())
                .is_some_and(|catalog| has_pages(&catalog)),
            Some(MaybeRef::NotRef(d)) => has_pages(&d),
            None => false,
        };
        if valid {
            trailer_dict = Some(dict);
        }
    }
    let has_encryption = trailer_dict
        .as_ref()
        .is_some_and(|t| t.contains_key(ENCRYPT));
    let input = match trailer_dict {
        Some(trailer_dict) => Some(XRefInput::TrailerDictData(trailer_dict.data())),
        None => root_ref.map(XRefInput::RootRef),
    };

    if has_encryption
        && decryptor.is_none()
        && let Some(input) = input
        && let Ok(decryptor) = table.read_decryptor(input, password)
    {
        // Without the decryptor, the objects of encrypted object streams were not
        // found: the table is rebuilt with it, for the same trailer.
        let (table, _) =
            fallback_xref_map_inner(data, walk, Some(Arc::new(decryptor)), password, old, unread);
        return (table, Some(input));
    }
    (table, input)
}

/// The places of the rebuilt table besides the headers the scan found, `found`, in
/// the order of the file: where each entry of the old table that leads to an object
/// leads (rule 8), or, where that is after the start of a header of `found` and
/// before where the object after it starts, at the same index of `bodies`, that
/// header's place (rule 7). [`merge`] keeps each entry at the place given here. Every
/// read of the rebuilt table, and of the rebuild's probe, reads a body only up to the
/// next place ([`Layout`]).
fn places(old: &Old, found: &[(ObjectIdentifier, usize)], bodies: &[usize]) -> Vec<usize> {
    old.led
        .iter()
        .map(|(_, at, header)| led_to(*at, *header, found, bodies).0)
        .collect()
}

/// Where an entry of the old table at `at`, which leads to an object after a header
/// naming `header`, leads in the rebuilt table, and the identifier the header there
/// names: the header of `found` that `at` is in, after its start and before where the
/// object after it starts, at the same index of `bodies` (rule 7); else `at`.
fn led_to(
    at: usize,
    header: ObjectIdentifier,
    found: &[(ObjectIdentifier, usize)],
    bodies: &[usize],
) -> (usize, ObjectIdentifier) {
    // The last header found that starts before `at`.
    let before = found.partition_point(|(_, start)| *start < at);
    match before.checked_sub(1) {
        Some(i) if at < bodies[i] => (found[i].1, found[i].0),
        _ => (at, header),
    }
}

/// What a rebuild finds at `r`: `None` where no header can start, as no number can;
/// else the header there, if any, and where the object after it starts if one does,
/// read to its end however far that is, as reading it does; `r` after the header.
/// The scan reads each part of the file a bounded number of times however many
/// objects it holds.
fn rebuilt_header(
    r: &mut Reader<'_>,
    scan: &mut Scan<'_>,
) -> Option<Option<(ObjectIdentifier, Option<usize>)>> {
    if !r.peek_byte().is_some_and(object::starts_number) {
        return None;
    }
    let Some(id) = scan_header(r) else {
        return Some(None);
    };
    let mut body = r.clone();
    body.skip_white_spaces_and_comments();
    let at = body.offset();
    Some(Some((id, scan.object(at).map(|_| at))))
}

/// The header `r` starts with, `N G obj`, and `r` after it.
fn scan_header(r: &mut Reader<'_>) -> Option<ObjectIdentifier> {
    let mut header = r.clone();
    let id = header.read_without_context::<ObjectIdentifier>();
    if id.is_some() {
        *r = header;
    }
    id
}

/// Merges the old table into the table the scan rebuilt, `xref_map`, once the probe
/// has read the object streams. `xref_map` holds each object at the last header the
/// scan found of it with an object after it (rule 5); `held` holds each definition the
/// scan found in an object stream: the object's number, the stream, by its index in
/// `streams`, and the index in it, in the order of the file. An entry into an object
/// stream names the stream the definition is in ([`EntryType::Found`]), whatever
/// definition of the stream's number the table takes. `found` and `bodies` are the
/// walk's ([`Walk`]),
/// `broken` the objects whose header it found with no object that can be read after
/// it, and `unread` the entries [`old_offsets`] found lead to no such object.
///
/// - Rule 6 decides which definition in object streams `xref_map` takes, where rule 5
///   gives an object a definition outside them by `xref_map` as it is on entry. The
///   stream the old table lists an object in is the one of `streams` at the place the
///   old entry of the stream's number leads to ([`places`]).
/// - Rule 8 then keeps the entries of `old` that lead to an object, each at the place
///   [`places`] gives it (rule 7), where `xref_map` holds no object of the entry's
///   number and generation, and decides what the number another's header names takes.
/// - Rule 9: the returned [`Unreadable`] holds those of `unread` and `broken` that
///   `xref_map` then holds no definition of.
///
/// Returns, for the place of each entry kept whose header names another object, that
/// object ([`Headers`]), and the unreadable objects.
#[allow(clippy::too_many_arguments)]
fn merge(
    xref_map: &mut XrefMap,
    held: &[(u32, usize, usize)],
    streams: &[(ObjectIdentifier, usize)],
    found: &[(ObjectIdentifier, usize)],
    bodies: &[usize],
    broken: &[ObjectIdentifier],
    old: &Old,
    unread: &[ObjectIdentifier],
) -> (Headers, Unreadable) {
    // Rules 7 and 8. Each entry that leads to an object, where it leads in the
    // rebuilt table and the identifier the header there names.
    let led: Vec<(ObjectIdentifier, usize, ObjectIdentifier)> = old
        .led
        .iter()
        .map(|&(id, at, header)| {
            let (place, header) = led_to(at, header, found, bodies);
            (id, place, header)
        })
        .collect();

    // Rule 6. The last definition of each number in an object stream, and the last in
    // the stream the old table lists it in, where that stream holds it.
    let mut last_held: hash::Map<u32, (usize, usize)> = hash::Map::default();
    let mut in_listed: hash::Map<u32, (usize, usize)> = hash::Map::default();
    if !held.is_empty() && !old.in_streams.is_empty() {
        // Where the old entry of each stream an entry into a stream names leads.
        let listed_streams: hash::Set<ObjectIdentifier> = old
            .in_streams
            .values()
            .filter_map(|stream| generation_0(*stream))
            .collect();
        let listed_at: hash::Map<ObjectIdentifier, usize> = led
            .iter()
            .filter(|(id, _, _)| listed_streams.contains(id))
            .map(|&(id, place, _)| (id, place))
            .collect();
        for &(number, stream, index) in held {
            let listed = generation_0(number)
                .and_then(|id| old.in_streams.get(&id))
                .and_then(|listed| generation_0(*listed))
                .and_then(|listed| listed_at.get(&listed));
            if listed == Some(&streams[stream].1) {
                in_listed.insert(number, (stream, index));
            }
        }
    }
    for &(number, stream, index) in held {
        last_held.insert(number, (stream, index));
    }
    // A number held more than once is decided alike each time: only the walk puts a
    // definition outside object streams in the table.
    for &(number, _, _) in held {
        let Some(id) = generation_0(number) else {
            continue;
        };
        let (stream, index) = match in_listed.get(&number) {
            Some(listed) => *listed,
            None if matches!(xref_map.get(&id), Some(EntryType::Normal(_))) => continue,
            None => last_held[&number],
        };
        if let Some(entry) = in_stream(stream, index) {
            xref_map.insert(id, entry);
        }
    }

    // Rules 7 and 8.
    let listed: hash::Set<(ObjectIdentifier, usize)> =
        led.iter().map(|&(id, place, _)| (id, place)).collect();
    let kept: Vec<(ObjectIdentifier, usize, ObjectIdentifier)> = led
        .into_iter()
        .filter(|(id, _, _)| !xref_map.contains_key(id))
        .collect();
    // The headers of other objects that kept entries take, where the old table does
    // not also list that object: the object takes a definition elsewhere.
    let taken: hash::Set<(ObjectIdentifier, usize)> = kept
        .iter()
        .filter(|&&(id, place, header)| header != id && !listed.contains(&(header, place)))
        .map(|&(_, place, header)| (header, place))
        .collect();
    if !taken.is_empty() {
        // The objects the table has at a header taken, each once.
        let mut moved: Vec<ObjectIdentifier> = vec![];
        let mut seen = hash::Set::default();
        for &(_, _, header) in &kept {
            let at_taken = matches!(
                xref_map.get(&header),
                Some(EntryType::Normal(at)) if taken.contains(&(header, *at))
            );
            if at_taken && seen.insert(header) {
                moved.push(header);
            }
        }
        // The last definition of each outside object streams, at no header taken.
        let mut elsewhere: hash::Map<ObjectIdentifier, usize> = hash::Map::default();
        for &(id, at) in found {
            if seen.contains(&id) && !taken.contains(&(id, at)) {
                elsewhere.insert(id, at);
            }
        }
        for id in moved {
            let in_a_stream = || {
                let number = u32::try_from(id.obj_number).ok()?;
                let (stream, index) = *last_held.get(&number).filter(|_| id.gen_number == 0)?;
                in_stream(stream, index)
            };
            match elsewhere
                .get(&id)
                .map(|at| EntryType::Normal(*at))
                .or_else(in_a_stream)
            {
                Some(entry) => xref_map.insert(id, entry),
                None => xref_map.remove(&id),
            };
        }
    }
    let mut headers = Headers::default();
    for (id, place, header) in kept {
        xref_map.insert(id, EntryType::Normal(place));
        if header != id {
            headers.insert(place, header);
        }
    }

    // Rule 9.
    let unreadable = unread
        .iter()
        .chain(broken)
        .filter(|id| !xref_map.contains_key(id))
        .copied()
        .collect();
    (headers, unreadable)
}

/// The identifier of number `number`, generation 0, as an object stream or an object
/// in one is named, where the number is one.
fn generation_0(number: u32) -> Option<ObjectIdentifier> {
    Some(ObjectIdentifier::new(i32::try_from(number).ok()?, 0))
}

/// The entry of the object at `index` of the object stream a rebuild found at index
/// `stream` of its streams ([`Found`]).
fn in_stream(stream: usize, index: usize) -> Option<EntryType> {
    Some(EntryType::Found(
        u32::try_from(stream).ok()?,
        u32::try_from(index).ok()?,
    ))
}

const DUMMY_XREF: XRef = XRef(Inner::Dummy);

#[cfg(test)]
std::thread_local! {
    /// While `Some`, how often the body of each object has been read on this thread,
    /// by the entry a read of it was made for, which the tests bound to show that
    /// loading the pages reads each object once.
    pub(crate) static RESOLVED: core::cell::RefCell<Option<hash::Map<ObjectIdentifier, usize>>> =
        const { core::cell::RefCell::new(None) };
}

/// An xref table.
#[derive(Debug, Clone)]
pub struct XRef(Inner);

/// What readers of a document apart from the table it was made from share with it
/// ([`XRef::detached`]), nothing read through it kept.
#[derive(Debug, Clone)]
pub struct Detached(XRef);

impl Detached {
    /// A reader of the document that keeps what its reads keep (the object streams
    /// they decode, the bodies they parse) in stores of its own, freed with it: so
    /// nothing read through it is kept by the table, nor by another reader.
    pub(crate) fn reader(&self) -> XRef {
        self.0.detached().0
    }

    /// The document the readers read, if any.
    pub(crate) fn document_id(&self) -> Option<DocumentId> {
        self.0.document_id()
    }

    /// The file the readers read.
    pub(crate) fn file_data(&self) -> Option<&PdfData> {
        self.0.file_data()
    }
}

impl XRef {
    /// The table `map` of the file `data`, with what reading it found besides,
    /// `rebuilt`, decrypted by `decryptor`, its trailer not read yet.
    fn table(
        data: PdfData,
        rebuilt: Rebuilt,
        map: Arc<XrefMap>,
        decryptor: Arc<Decryptor>,
    ) -> Self {
        Self(Inner::Some(Arc::new(SomeRepr {
            document: DocumentId::new(),
            data: Arc::new(Data::new(data)),
            layout: rebuilt.layout,
            map,
            repairs: Arc::new(rebuilt.repairs),
            shapes: Arc::default(),
            decryptor,
            has_ocgs: false,
            metadata: Arc::new(Metadata::default()),
            trailer_data: TrailerData::dummy(),
        })))
    }

    /// The table's entries, which no other copy of it holds.
    fn into_map(self) -> XrefMap {
        let Inner::Some(repr) = self.0 else {
            unreachable!()
        };
        Arc::unwrap_or_clone(Arc::unwrap_or_clone(repr).map)
    }

    /// What decrypts the document.
    fn decryptor(&self) -> Arc<Decryptor> {
        match &self.0 {
            Inner::Some(repr) => repr.decryptor.clone(),
            Inner::Dummy => Arc::new(Decryptor::None),
        }
    }

    /// What decrypts the document whose trailer is `input`, as its /Encrypt says,
    /// read through this table, which decrypts nothing ([`Decryptor::None`]).
    fn read_decryptor(
        &self,
        input: XRefInput<'_>,
        password: &[u8],
    ) -> Result<Decryptor, XRefError> {
        match input {
            XRefInput::TrailerDictData(trailer_dict_data) => {
                let trailer_dict = Reader::new(trailer_dict_data)
                    .read_with_context::<Dict<'_>>(&ReaderContext::new(self, false))
                    .ok_or(XRefError::Unknown)?;
                get_decryptor(&trailer_dict, password)
            }
            XRefInput::RootRef(_) => Ok(Decryptor::None),
        }
    }

    /// The table `xref` of the file, its trailer read from `input`, decrypted as the
    /// trailer's /Encrypt, read without decrypting, says. What the table read before
    /// is kept where it was read with that decryptor, as a rebuilt table reads its
    /// catalog before (`fallback_xref_map_inner`): the bodies read with another are
    /// dropped.
    fn new(mut xref: Self, input: XRefInput<'_>, password: &[u8]) -> Result<Self, XRefError> {
        // The trailer is read twice, once to determine the encryption used and then a
        // second time to resolve the catalog dictionary, etc. This allows us to support
        // catalog dictionaries that are stored in an encrypted object stream.
        let read_with = xref.decryptor();
        xref.repr_mut().decryptor = Arc::new(Decryptor::None);
        let decryptor = xref.read_decryptor(input, password)?;
        let repr = xref.repr_mut();
        if *read_with == decryptor {
            repr.decryptor = read_with;
        } else {
            repr.decryptor = Arc::new(decryptor);
            repr.shapes = Arc::default();
            repr.data = Arc::new(Data::new(repr.data.get().clone()));
        }

        let (trailer_data, has_ocgs, metadata) = match input {
            XRefInput::TrailerDictData(trailer_dict_data) => {
                let mut r = Reader::new(trailer_dict_data);

                let trailer_dict = r
                    .read_with_context::<Dict<'_>>(&ReaderContext::new(&xref, false))
                    .ok_or(XRefError::Unknown)?;

                let root_ref = trailer_dict.get_ref(ROOT).ok_or(XRefError::Unknown)?;
                let root = trailer_dict
                    .get::<Dict<'_>>(ROOT)
                    .ok_or(XRefError::Unknown)?;
                let metadata = trailer_dict
                    .get::<Dict<'_>>(INFO)
                    .map(|d| parse_metadata(&d))
                    .unwrap_or_default();
                let has_ocgs = root.get::<Dict<'_>>(OCPROPERTIES).is_some();
                let version = root
                    .get::<Name<'_>>(VERSION)
                    .and_then(|v| PdfVersion::from_bytes(v.deref()));

                let td = TrailerData {
                    root_ref: root_ref.into(),
                    version,
                };

                (td, has_ocgs, metadata)
            }
            XRefInput::RootRef(root_ref) => {
                xref.get::<Dict<'_>>(root_ref).ok_or(XRefError::Unknown)?;

                let td = TrailerData {
                    root_ref,
                    version: None,
                };

                (td, false, Metadata::default())
            }
        };

        let repr = xref.repr_mut();
        repr.trailer_data = trailer_data;
        repr.has_ocgs = has_ocgs;
        repr.metadata = Arc::new(metadata);

        Ok(xref)
    }

    /// The table's own copy of what it holds.
    fn repr_mut(&mut self) -> &mut SomeRepr {
        match &mut self.0 {
            Inner::Dummy => unreachable!(),
            Inner::Some(repr) => Arc::make_mut(repr),
        }
    }

    /// What readers of this table's document apart from it share with it, from which
    /// [`Detached::reader`] makes each: the file, its entries, and where its objects
    /// and its `endstream`s are. Where each `endstream` is is found once for the
    /// document; found by a reader apart, it is kept in common with this table, at
    /// [`Self::apart_bytes`].
    pub fn detached(&self) -> Detached {
        Detached(match &self.0 {
            Inner::Dummy => Self(Inner::Dummy),
            Inner::Some(repr) => Self(Inner::Some(Arc::new(SomeRepr {
                data: Arc::new(Data::new(repr.data.get().clone())),
                shapes: Arc::default(),
                layout: Layout {
                    starts: repr.layout.starts.clone(),
                    ends: repr.layout.ends.apart(),
                },
                ..SomeRepr::clone(repr)
            }))),
        })
    }

    /// The bytes that readers apart from this table ([`Self::detached`]) keep in
    /// common with it, which no read through it allocated: where each `endstream` of
    /// the file is, if one of them looked for it before a read through the table did.
    pub fn apart_bytes(&self) -> usize {
        match &self.0 {
            Inner::Dummy => 0,
            Inner::Some(repr) => repr.layout.ends.apart_bytes(),
        }
    }

    pub(crate) fn dummy() -> &'static Self {
        &DUMMY_XREF
    }

    pub(crate) fn len(&self) -> usize {
        match &self.0 {
            Inner::Dummy => 0,
            Inner::Some(r) => r.map.len(),
        }
    }

    pub(crate) fn trailer_data(&self) -> &TrailerData {
        match &self.0 {
            Inner::Dummy => unreachable!(),
            Inner::Some(r) => &r.trailer_data,
        }
    }

    pub(crate) fn metadata(&self) -> &Metadata {
        match &self.0 {
            Inner::Dummy => unreachable!(),
            Inner::Some(r) => &r.metadata,
        }
    }

    /// The document's identifier, unless this is no document's table or the target
    /// cannot count documents.
    pub(crate) fn document_id(&self) -> Option<DocumentId> {
        match &self.0 {
            Inner::Dummy => None,
            Inner::Some(r) => r.document,
        }
    }

    /// Return the object ID of the root dictionary.
    pub fn root_id(&self) -> ObjectIdentifier {
        self.trailer_data().root_ref
    }

    /// Whether the PDF has optional content groups.
    pub fn has_optional_content_groups(&self) -> bool {
        match &self.0 {
            Inner::Dummy => false,
            Inner::Some(r) => r.has_ocgs,
        }
    }

    pub(crate) fn objects(&self) -> impl IntoIterator<Item = Object<'_>> + '_ {
        let mut iter = self.ids_in_file_order();
        let ctx = ReaderContext::new(self, false);

        iter::from_fn(move || {
            for next in iter.by_ref() {
                if let Some(obj) = self.get_parsed(next, &ctx) {
                    return Some(obj);
                } else {
                    // Skip invalid objects.
                    continue;
                }
            }

            None
        })
    }

    /// The objects the table lists, in the order the file holds them, an object of
    /// an object stream at its stream's place; entries leading to one place come in
    /// no set order among themselves.
    pub(crate) fn ids_in_file_order(&self) -> impl Iterator<Item = ObjectIdentifier> + use<> {
        match &self.0 {
            Inner::Dummy => unimplemented!(),
            Inner::Some(r) => {
                let mut elements = r
                    .map
                    .iter()
                    .map(|(id, e)| {
                        let offset = match e {
                            EntryType::Normal(o) => (*o, 0),
                            EntryType::ObjStream(id, index) => {
                                if let Some(EntryType::Normal(offset)) =
                                    r.map.get(&ObjectIdentifier::new(*id as i32, 0))
                                {
                                    (*offset, *index)
                                } else {
                                    (usize::MAX, 0)
                                }
                            }
                            EntryType::Found(stream, index) => {
                                (r.repairs.streams[*stream as usize].1, *index)
                            }
                        };

                        (*id, offset)
                    })
                    .collect::<Vec<_>>();

                elements.sort_by_key(|element| element.1);
                elements.into_iter().map(|(id, _)| id)
            }
        }
    }

    /// Whether the document is encrypted.
    #[cfg(test)]
    pub(crate) fn is_encrypted(&self) -> bool {
        match &self.0 {
            Inner::Dummy => false,
            Inner::Some(r) => !matches!(r.decryptor.as_ref(), Decryptor::None),
        }
    }

    pub(crate) fn needs_decryption(&self, ctx: &ReaderContext<'_>) -> bool {
        match &self.0 {
            Inner::Dummy => false,
            Inner::Some(r) => {
                if matches!(r.decryptor.as_ref(), Decryptor::None) {
                    false
                } else {
                    !ctx.in_content_stream() && !ctx.in_object_stream()
                }
            }
        }
    }

    pub(crate) fn decrypt(
        &self,
        id: ObjectIdentifier,
        data: &[u8],
        target: DecryptionTarget<'_>,
    ) -> Option<Vec<u8>> {
        crate::work::told(data.len());
        match &self.0 {
            Inner::Dummy => Some(data.to_vec()),
            Inner::Some(r) => r.decryptor.decrypt(id, data, target),
        }
    }

    /// Whether stream `data` decrypts whole (see [`Decryptor::decrypts_whole`]).
    pub(crate) fn decrypts_whole(&self, data: &[u8], target: DecryptionTarget<'_>) -> bool {
        match &self.0 {
            Inner::Dummy => true,
            Inner::Some(r) => r.decryptor.decrypts_whole(data, target),
        }
    }

    /// A reader of stream `data` of object `id`, decrypted; `None` where
    /// [`Self::decrypt`] would fail.
    pub(crate) fn decrypted_reader<'a>(
        &self,
        id: ObjectIdentifier,
        data: &'a [u8],
        target: DecryptionTarget<'_>,
    ) -> Option<DecryptedReader<'a>> {
        match &self.0 {
            Inner::Dummy => Some(DecryptedReader::raw(Cow::Borrowed(data))),
            Inner::Some(r) => r.decryptor.reader(id, data, target),
        }
    }

    /// The document's bytes.
    #[cfg(feature = "images")]
    pub(crate) fn file_data(&self) -> Option<&PdfData> {
        match &self.0 {
            Inner::Some(repr) => Some(repr.data.get()),
            Inner::Dummy => None,
        }
    }

    /// Whether the table lists object `id`, whether or not it can be read; a reference
    /// to an object it does not list is `null`. A table rebuilt from a damaged file
    /// lists an object whose header is in the file, or that the damaged table lists at
    /// a header, though no object can be read after it.
    pub fn has_entry(&self, id: ObjectIdentifier) -> bool {
        match &self.0 {
            Inner::Dummy => false,
            Inner::Some(r) => r.map.contains_key(&id) || r.repairs.unreadable.contains(&id),
        }
    }

    /// Identifies the body entry `id` leads to, which every entry leading to its
    /// place reads ([`Place`]): its bytes, from the place to the end of its slot, the
    /// document, and the object whose key decrypts it. For a cache of what is copied
    /// from objects, so that entries leading to one body share one copy, read and
    /// written once. Nothing of the body is read, only, for an object of an object
    /// stream, the stream, once for all its objects. `None` where the table does not
    /// list `id`, and where it lists it but its place is lost, which the computation
    /// being watched is told ([`Self::watch`]).
    pub fn content_key(&self, id: ObjectIdentifier) -> Option<ContentKey<'_>> {
        let ctx = ReaderContext::new(self, false);
        let (repr, entry, entered) = self.entry(id, &ctx)?;
        match self.place(repr, id, entry, &entered) {
            Ok((place, data, slot)) => match data.get(place.at..slot.end) {
                Some(body) => Some(ContentKey::new(body, &entered)),
                // Past the bytes: nothing can be read there, which the read tells.
                None => self.read::<Object<'_>>(id, &ctx, Taken::Either).and(None),
            },
            Err(Unread::Rejected) => None,
            Err(Unread::Lost(lost)) => {
                self.note(Some(lost));
                None
            }
        }
    }

    /// An object the table lists that cannot be read, if there is one.
    pub(crate) fn unreadable_object(&self) -> Lost {
        match &self.0 {
            Inner::Dummy => None,
            Inner::Some(r) => r.repairs.unreadable.first().copied(),
        }
    }

    /// Return the object with the given identifier; `None` too for one the table
    /// lists but that cannot be read, which is lost ([`Self::watch`]).
    #[allow(private_bounds)]
    pub fn get<'a, T>(&'a self, id: ObjectIdentifier) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        let ctx = ReaderContext::new(self, false);
        self.get_with(id, &ctx)
    }

    /// Runs `f`, and returns with its result what its reads of this document lost
    /// ([`crate::lost`]): the first object they met that the table lists but that
    /// cannot be read, however they read it, and whatever a value computed before
    /// and kept tells ([`Self::note`]). The computation `f` runs within, if any, is
    /// told too.
    pub fn watch<T>(&self, f: impl FnOnce() -> T) -> (T, Lost) {
        match &self.0 {
            Inner::Some(repr) => repr.data.watched.watch(true, f),
            Inner::Dummy => (f(), None),
        }
    }

    /// Tells the computation being watched that it lost `lost`, as a value it uses
    /// that was computed before, and lost it then, does ([`crate::lost::Memo`]).
    pub fn note(&self, lost: Lost) {
        if let Inner::Some(repr) = &self.0 {
            repr.data.watched.note(lost);
        }
    }

    /// [`Self::watch`], but the computation `f` runs within is not told: for a value
    /// kept for others, which tell what it lost where they use it.
    pub fn apart<T>(&self, f: impl FnOnce() -> T) -> (T, Lost) {
        match &self.0 {
            Inner::Some(repr) => repr.data.watched.watch(false, f),
            Inner::Dummy => (f(), None),
        }
    }

    /// Object `id` read from `ctx` as a `T`, or `Err` with what reading it lost
    /// ([`Self::watch`]), which the computation around is told too.
    pub(crate) fn get_checked_with<'a, T>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Result<Option<T>, ObjectIdentifier>
    where
        T: ObjectLike<'a>,
    {
        match self.watch(|| self.get_with::<T>(id, ctx)) {
            (_, Some(lost)) => Err(lost),
            (object, None) => Ok(object),
        }
    }

    /// Object `id` as a `T`, read from `ctx`: `None` where the table does not list
    /// it, where `ctx`'s chain rejects it as a cycle, or where it is another kind of
    /// object. Where the table lists it but it cannot be read (as a `T` that is read
    /// with a stream's data, a stream whose data cannot be read cannot), it is `None`
    /// too, and lost: the computation being watched is told ([`Self::watch`]).
    ///
    /// The object is the one its body holds ([`Self::read`]), as a `T`: a stream, for
    /// any `T` but a stream or any object, as its dictionary.
    #[allow(private_bounds)]
    pub(crate) fn get_with<'a, T>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        let taken = if T::STREAM { Taken::Whole } else { Taken::Dict };
        self.read::<Object<'a>>(id, ctx, taken)?.cast()
    }

    /// Object `id`, read from `ctx` as [`Self::get_with`] reads any object, but a
    /// stream whose data cannot be read is its dictionary, as each place this is
    /// used reads it as a dictionary, if not as another kind of object.
    pub(crate) fn get_parsed<'a>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Option<Object<'a>> {
        self.read::<Object<'a>>(id, ctx, Taken::Either)
    }

    /// Object `id` without the data of a stream, of which only the dictionary is
    /// read ([`Shallow`]): for a reader that asks of a stream only its entries,
    /// which a stream whose data cannot be read still has.
    pub fn get_shallow(&self, id: ObjectIdentifier) -> Option<Shallow<'_>> {
        let ctx = ReaderContext::new(self, false);
        self.get_shallow_with(id, &ctx)
    }

    /// [`Self::get_shallow`], read from `ctx`.
    pub(crate) fn get_shallow_with<'a>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Option<Shallow<'a>> {
        self.read::<Shallow<'a>>(id, ctx, Taken::Dict)
    }

    /// The object with the given identifier as the file writes it ([`Written`]), for
    /// a copy: `None` where [`Self::get`] reads no object, a stream whose data cannot
    /// be read included, which is lost.
    pub fn written(&self, id: ObjectIdentifier) -> Option<Written<'_>> {
        let ctx = ReaderContext::new(self, false);
        self.read::<Written<'_>>(id, &ctx, Taken::Whole)
    }

    /// The object entry `id` leads to, read from `ctx`, a stream taken as `taken`
    /// says; `None` where the table does not list it, where `ctx`'s chain rejects its
    /// body or what reading it needs as a cycle ([`Link`](crate::reader::Link)), or
    /// where it is lost, which the computation being watched is told: every read of
    /// an object goes through here.
    fn read<'a, O: Made<'a>>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
        taken: Taken,
    ) -> Option<O> {
        let (repr, entry, entered) = self.entry(id, ctx)?;
        match self.body(repr, id, entry, ctx, entered, taken) {
            Ok(object) => Some(object),
            Err(Unread::Rejected) => None,
            Err(Unread::Lost(lost)) => {
                self.note(Some(lost));
                None
            }
        }
    }

    /// The object entry `id`, at `entry`, leads to, read in `entered`, the context of
    /// the entry from `ctx` ([`Self::entry`]): the object its body holds, read once
    /// for every entry leading to the same place and every read. A body `ctx`'s chain
    /// is reading already is a cycle, through whichever entry it is reached.
    ///
    /// `entered` is taken, not borrowed, so that the link is added to the context
    /// [`Self::entry`] made rather than to a copy of it.
    fn body<'a, O: Made<'a>>(
        &'a self,
        repr: &'a SomeRepr,
        id: ObjectIdentifier,
        entry: EntryType,
        ctx: &ReaderContext<'a>,
        mut entered: ReaderContext<'a>,
        taken: Taken,
    ) -> Result<O, Unread> {
        let (place, data, slot) = self.place(repr, id, entry, &entered)?;
        let link = place.link();
        if ctx.rejects(&link) {
            warn!("cycle detected in indirect object: {id:?}");

            return Err(Unread::Rejected);
        }
        entered.parent_chain_push(link);
        self.at_place(repr, id, entry, (place, data, slot), ctx, &entered, taken)
    }

    /// The object the body at `place`, in `data`, whose slot ends at `slot`, holds,
    /// read for entry `id`, at `entry`, in `entered`, the context of the entry from
    /// `ctx` whose chain holds the body: read once for every entry leading there and
    /// every read.
    #[allow(clippy::too_many_arguments)]
    fn at_place<'a, O: Made<'a>>(
        &'a self,
        repr: &'a SomeRepr,
        id: ObjectIdentifier,
        entry: EntryType,
        (place, data, slot): (Place, &'a [u8], Slot),
        ctx: &ReaderContext<'a>,
        entered: &ReaderContext<'a>,
        taken: Taken,
    ) -> Result<O, Unread> {
        let kept = repr.shapes.get().whole(&place);
        let shape = match kept {
            Some(shape) => shape,
            None => {
                #[cfg(test)]
                RESOLVED.with_borrow_mut(|resolved| {
                    if let Some(resolved) = resolved {
                        *resolved.entry(id).or_default() += 1;
                    }
                });
                // Reading a body resolves nothing: no lock is held while it reads.
                let shape = body::Shape::read(data, slot, place, entered);
                repr.shapes.get().read(place, shape)
            }
        };
        match shape {
            body::Shape::Unreadable => Err(Unread::Lost(id)),
            body::Shape::Value(top) => Ok(O::value(&top, data, entered)),
            body::Shape::Stream {
                dict: shape,
                end,
                data: kept,
            } => {
                let dict = shape.dict(data, entered);
                if taken == Taken::Dict {
                    return Ok(O::dict(dict));
                }
                let found = match kept {
                    Some((found, lost)) => {
                        self.note(lost);
                        found
                    }
                    None => {
                        // The data are found by /Length, which may be an object that
                        // the chain of references reading this one rejects: they are
                        // kept for every chain only where none does.
                        let ((found, lost), shared) = ctx.shared(|ctx| {
                            self.watch(|| {
                                let mut entered = Self::entered(ctx, id, repr, entry);
                                entered.parent_chain_push(place.link());
                                let dict = shape.dict(data, &entered);
                                repr.stream_data(data, place, slot, end, &dict)
                            })
                        });
                        if shared {
                            repr.shapes.get().found(&place, (found, lost));
                        }
                        found
                    }
                };
                match (found, taken) {
                    (Some((from, _, cut)), Taken::Contained) => {
                        Ok(O::stream(Stream::new(&data[from..cut], dict)))
                    }
                    (Some((from, to, _)), _) => Ok(O::stream(Stream::new(&data[from..to], dict))),
                    (None, Taken::Either) => Ok(O::dict(dict)),
                    (None, _) => Err(Unread::Lost(id)),
                }
            }
        }
    }

    /// Where entry `id`, at `entry`, leads, read in `entered`, the bytes the place is
    /// in, and where in them its slot ends: at the next place in them ([`Place`],
    /// [`Layout`], [`ObjectStream::slot`]). This reads nothing of the file but,
    /// for an object in an object stream, the stream, once ([`Self::object_stream`]):
    /// the body is read at the place ([`body::Shape::read`]).
    fn place<'a>(
        &'a self,
        repr: &'a SomeRepr,
        id: ObjectIdentifier,
        entry: EntryType,
        entered: &ReaderContext<'a>,
    ) -> Result<(Place, &'a [u8], Slot), Unread> {
        let decryption = entered.decryption();
        match entry {
            EntryType::Normal(at) => {
                let data = repr.data.get().as_ref();
                let place = Place {
                    source: Source::File,
                    at,
                    decryption,
                };
                Ok((place, data, Slot::to(repr.layout.slot_end(data, at))))
            }
            EntryType::ObjStream(..) | EntryType::Found(..) => {
                let (stream, at, index) = match entry {
                    EntryType::ObjStream(stream, index) => {
                        // Generation number is implicitly 0.
                        let stream = ObjectIdentifier::new(stream as i32, 0);
                        // A table that is kept lists each stream it puts objects in
                        // in the file ([`check`]).
                        let Some(&EntryType::Normal(at)) = repr.map.get(&stream) else {
                            return Err(Unread::Lost(stream));
                        };
                        (stream, at, index)
                    }
                    EntryType::Found(stream, index) => {
                        let (header, at) = repr.repairs.streams[stream as usize];
                        (header, at, index)
                    }
                    EntryType::Normal(_) => unreachable!(),
                };
                let (kept, objects) = self.object_stream(repr, stream, at, entered)?;
                let at = objects.offset(id, index).ok_or(Unread::Lost(id))?;
                let place = Place {
                    // The list keeping object streams holds fewer than 2^32.
                    source: Source::Stream(kept as u32),
                    at,
                    decryption,
                };
                Ok((place, objects.data(), objects.slot(at)))
            }
        }
    }

    /// Object stream `stream`, at `at` of the file, read from `ctx`, decoded once for
    /// every entry leading to its place ([`Self::decoded`]), and where it is kept. A
    /// stream that cannot be read is lost, with the objects in it.
    fn object_stream<'a>(
        &'a self,
        repr: &'a SomeRepr,
        stream: ObjectIdentifier,
        at: usize,
        ctx: &ReaderContext<'a>,
    ) -> Result<(usize, &'a ObjectStream), Unread> {
        let entry = EntryType::Normal(at);
        let entered = Self::entered(ctx, stream, repr, entry);
        let place = self.place(repr, stream, entry, &entered)?;
        self.decoded(repr, stream, entry, place, ctx)
    }

    /// The object stream entry `stream`, at `entry`, leads to, at `place`, read from
    /// `ctx` as [`Self::get_with`] reads a stream, its data cut at the place of
    /// another stream they reach ([`Taken::Contained`]), decoded, and where it is
    /// kept: once for every entry and every read leading to the place, and for the
    /// rebuild of a table, which reads its object streams by their places
    /// ([`Self::object_stream_at`]). What reading it lost is told to every read of it
    /// ([`crate::data::Data::object_stream`]).
    fn decoded<'a>(
        &'a self,
        repr: &'a SomeRepr,
        stream: ObjectIdentifier,
        entry: EntryType,
        place: (Place, &'a [u8], Slot),
        ctx: &ReaderContext<'a>,
    ) -> Result<(usize, &'a ObjectStream), Unread> {
        let decoded = repr.data.object_stream(place.0, stream, ctx, |ctx| {
            let link = place.0.link();
            if ctx.rejects(&link) {
                return None;
            }
            let mut entered = Self::entered(ctx, stream, repr, entry);
            entered.parent_chain_push(link);
            let read = |taken| {
                self.at_place::<Object<'_>>(repr, stream, entry, place, ctx, &entered, taken)
            };
            match read(Taken::Contained) {
                Ok(Object::Stream(contained)) => {
                    // Cut data end where the stream's go on.
                    let cut = matches!(
                        read(Taken::Whole),
                        Ok(Object::Stream(whole))
                            if whole.stored().len() > contained.stored().len()
                    );
                    ObjectStream::new(&contained, cut)
                }
                Ok(_) | Err(Unread::Rejected) => None,
                Err(Unread::Lost(lost)) => {
                    self.note(Some(lost));
                    None
                }
            }
        });
        match decoded {
            Decoded::Stream(kept, objects) => Ok((kept, objects)),
            Decoded::Lost(lost) => Err(Unread::Lost(lost)),
            Decoded::Rejected => Err(Unread::Rejected),
        }
    }

    /// The object stream at `at` of the file, under header `header`, read as an entry
    /// leading there reads it ([`Self::decoded`]): how the rebuild of a table reads the
    /// object streams it finds, whatever entries the table ends with.
    fn object_stream_at(&self, at: usize, header: ObjectIdentifier) -> Option<&ObjectStream> {
        let Inner::Some(repr) = &self.0 else {
            return None;
        };
        let ctx = ReaderContext::new(self, false);
        let (_, objects) = self.object_stream(repr, header, at, &ctx).ok()?;
        Some(objects)
    }

    /// The entry of object `id`, and the context to find its place in, reached from
    /// `ctx`: what is read from it is decrypted as its header says and named `id`,
    /// the entry it is reached through, but checked for cycles by its body
    /// ([`Self::body`]). An object the table lists but that cannot be read has no
    /// entry, and is lost ([`Self::watch`]).
    fn entry<'a>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Option<(&'a SomeRepr, EntryType, ReaderContext<'a>)> {
        let Inner::Some(repr) = &self.0 else {
            return None;
        };

        // References to undefined objects are treated as null.
        let Some(&entry) = repr.map.get(&id) else {
            if repr.repairs.unreadable.contains(&id) {
                self.note(Some(id));
            }
            return None;
        };

        Some((repr, entry, Self::entered(ctx, id, repr, entry)))
    }

    /// The context entry `id`, at `entry`, is read in from `ctx`, but for the link to
    /// what it reads, which the caller adds to the chain once it knows it
    /// ([`Link`](crate::reader::Link)).
    ///
    /// What is read is decrypted as the object its header names, as it was encrypted:
    /// where entries of a damaged table lead to one object, each reads what the bytes
    /// hold, not bytes decrypted with a key they were not encrypted with, and the
    /// bytes alone say how they decrypt, so what is cached by them is what every entry
    /// reads ([`ContentKey`], [`Place`]). The header each entry leads to is found once,
    /// where the table is read: no entry reads it again.
    fn entered<'a>(
        ctx: &ReaderContext<'a>,
        id: ObjectIdentifier,
        repr: &SomeRepr,
        entry: EntryType,
    ) -> ReaderContext<'a> {
        let mut ctx = ctx.clone();
        ctx.set_obj_number(id);
        if let EntryType::Normal(at) = entry
            && let Some(header) = repr.repairs.headers.get(&at)
        {
            ctx.set_decryption(*header);
        }
        ctx.set_in_content_stream(false);
        ctx.set_in_object_stream(!matches!(entry, EntryType::Normal(_)));
        ctx
    }
}

/// Where the objects of a file start, each once, in order, and where `endstream` is
/// in it, found the first time a stream's /Length does not lead to one: what finding
/// a body's slot and a stream's data read ([`Place`]).
#[derive(Debug, Clone, Default)]
struct Layout {
    /// Where the slot of each object ends.
    starts: Arc<[usize]>,
    /// Where each `endstream` is, and where the data before it end, without the
    /// white space before it.
    ends: Ends,
}

impl Layout {
    /// The layout of the file whose objects are at `starts`.
    fn over(mut starts: Vec<usize>) -> Self {
        starts.sort_unstable();
        Self::sorted(starts.into_iter())
    }

    /// The layout of the file whose objects are at `starts`, in rising order.
    fn sorted(starts: impl Iterator<Item = usize>) -> Self {
        let mut starts: Vec<usize> = starts.collect();
        starts.dedup();
        Self {
            starts: starts.into(),
            ends: Ends::default(),
        }
    }

    /// Where the slot of the object at `at` of the file, `data`, ends ([`Place`]).
    fn slot_end(&self, data: &[u8], at: usize) -> usize {
        let next = self.starts.partition_point(|start| *start <= at);
        self.starts
            .get(next)
            .map_or(data.len(), |next| *next)
            .min(data.len())
    }

    /// Whether the object at `at` of the file, `data`, read in its slot, is a
    /// stream, as reading it finds ([`body::Read::in_slot`]).
    fn holds_stream(&self, data: &[u8], at: usize) -> bool {
        let (read, looked) = body::Read::in_slot(
            data,
            at,
            Slot::to(self.slot_end(data, at)),
            true,
            |r| r.skip::<Dict<'_>>(false),
            |r| r.skip::<Object<'_>>(false),
        );
        crate::work::read_bodies(looked);
        matches!(read, Some((body::Read::Stream { .. }, _)))
    }

    /// The data of the stream at `at` of the file, `data`, whose dictionary, `dict`,
    /// ends at `end`, as [`Stream::after_dict`] finds them, where they are: those
    /// /Length leads to, else up to the first `endstream` after the keyword
    /// `stream`, which is in the slot, the body's. And where they are cut, for an
    /// object stream: at the next place that holds a stream, if they reach it.
    ///
    /// The data are neither lexed nor copied, and where white space before an
    /// `endstream` runs long, or no /Length leads to one, it is found among where
    /// each is in the file, found once, with the white space before it: so finding
    /// the data of every stream of a file takes time linear in it, whatever data
    /// overlap. Where they are cut is read from the places after this one up to the
    /// first that holds a stream, which no other stream reads: a stream before this
    /// one stops at it.
    fn stream_data(
        &self,
        data: &[u8],
        at: usize,
        end: usize,
        dict: &Dict<'_>,
    ) -> Option<body::DataAt> {
        let mut r = Reader::new(data);
        r.jump(end);
        let ends = |r: &mut Reader<'_>| {
            let stop = endstream_past_white(data, r.offset(), &self.ends)?;
            r.jump(stop + b"endstream".len());
            Some(())
        };
        let (from, to, read) = match Stream::by_length_ending(&mut r, dict, ends)? {
            Some(stream) => {
                let from = stream.stored().as_ptr() as usize - data.as_ptr() as usize;
                let to = from + stream.stored().len();
                (
                    from,
                    to,
                    (from - end) + (r.offset() - to).min(NEAR + b"endstream".len()),
                )
            }
            None => {
                warn!("failed to parse stream, trying to parse it manually");
                let keyword = end
                    + object::stream::searched(data.get(end..self.slot_end(data, at))?, b"stream")?;
                let mut r = Reader::new(data);
                r.jump(keyword + b"stream".len());
                r.forward_tag(b"\n")
                    .or_else(|| r.forward_tag(b"\r\n"))
                    .or_else(|| r.forward_tag(b"\r"))?;
                let from = r.offset();
                let (_, trimmed) = first_endstream(&self.ends, data, from)?;
                (from, trimmed.max(from), from - end)
            }
        };
        crate::work::read_bodies(read);
        let after = &self.starts[self.starts.partition_point(|start| *start <= at)..];
        let cut = after
            .iter()
            .take_while(|start| **start < to)
            .find(|start| self.holds_stream(data, **start))
            .map_or(to, |start| (*start).clamp(from, to));
        Some((from, to, cut))
    }
}

/// Where each `endstream` of a file is, and where the data before it end, without
/// the white space before it ([`Layout`]): found once for the document, the first
/// time one is looked for, by a read through its table or through a reader apart
/// from it ([`XRef::detached`]). What a read through the table finds, the table
/// keeps; what a reader apart finds, every reader apart from the table shares with
/// it, so that what the table holds grows only through reads through it, and each
/// looks up what the other found.
#[derive(Debug, Clone, Default)]
struct Ends {
    found: Arc<FoundEnds>,
    /// Whether these are a reader's apart from the table, which finds into
    /// [`FoundEnds::apart`].
    is_apart: bool,
}

#[derive(Debug, Default)]
struct FoundEnds {
    /// What reads through the table found.
    table: OnceLock<Vec<(usize, usize)>>,
    /// What reads through readers apart from the table found.
    apart: OnceLock<Vec<(usize, usize)>>,
}

impl Ends {
    /// Where each `endstream` is, found by `find` unless found before.
    fn found(&self, find: impl FnOnce() -> Vec<(usize, usize)>) -> &[(usize, usize)] {
        let FoundEnds { table, apart } = &*self.found;
        if let Some(found) = table.get().or_else(|| apart.get()) {
            return found;
        }
        if self.is_apart {
            apart.get_or_init(find)
        } else {
            table.get_or_init(find)
        }
    }

    /// These, for a reader apart from the table.
    fn apart(&self) -> Self {
        Self {
            is_apart: true,
            ..self.clone()
        }
    }

    /// The bytes what readers apart from the table found takes.
    fn apart_bytes(&self) -> usize {
        self.found
            .apart
            .get()
            .map_or(0, |found| found.capacity() * size_of::<(usize, usize)>())
    }
}

/// The white space after a stream's data that is read before `endstream` is looked
/// for among where it is in the file ([`Layout::stream_data`]).
const NEAR: usize = 64;

impl SomeRepr {
    /// The data of the stream at `place`, whose dictionary, `dict`, ends at `end` of
    /// `data`, where the place's slot ends at `slot` ([`Layout::stream_data`]). In
    /// an object stream, where no stream belongs, the data end in the slot.
    fn stream_data(
        &self,
        data: &[u8],
        place: Place,
        slot: Slot,
        end: usize,
        dict: &Dict<'_>,
    ) -> Option<body::DataAt> {
        #[cfg(test)]
        body::DATA_FOUND.with_borrow_mut(|found| *found.entry(place).or_default() += 1);
        if place.source == Source::File {
            return self.layout.stream_data(data, place.at, end, dict);
        }
        let mut r = Reader::new(&data[..slot.end]);
        r.jump(end);
        let stream = match Stream::by_length(&mut r, dict)? {
            Some(stream) => stream,
            None => {
                r.jump(end);
                Stream::searched_for(&mut r, dict)?
            }
        };
        crate::work::read_bodies(r.offset().saturating_sub(end));
        let from = stream.stored().as_ptr() as usize - data.as_ptr() as usize;
        let to = from + stream.stored().len();
        Some((from, to, to))
    }
}

/// An input that is passed to the xref constructor so that we can fully resolve
/// the PDF.
#[derive(Debug, Copy, Clone)]
pub(crate) enum XRefInput<'a> {
    /// This option is going to be uesd in 99.999% of the case. It contains the
    /// raw data of the trailer dictionary which is then going to be processed.
    TrailerDictData(&'a [u8]),
    /// In case the trailer dictionary could not be read (for example because
    /// it is cut-off), we just pass the object ID of the root dictionary
    /// in case we have found one, and try our best to build the PDF just
    /// with the information we have there.
    ///
    /// Note that this won't work if the document is encrypted, as we
    /// can't access the crypto dictionary.
    RootRef(ObjectIdentifier),
}

/// What a read of an object makes of what its body holds: the object, or the
/// object as the file writes it ([`Written`]).
/// An object read without the data of a stream ([`XRef::get_shallow`]).
#[derive(Debug, Clone)]
pub enum Shallow<'a> {
    /// An object that is no stream.
    Value(Object<'a>),
    /// A stream's dictionary.
    Stream(Dict<'a>),
}

impl<'a> Shallow<'a> {
    /// The object, a stream as its dictionary.
    pub fn into_object(self) -> Object<'a> {
        match self {
            Self::Value(object) => object,
            Self::Stream(dict) => Object::Dict(dict),
        }
    }
}

impl<'a> Made<'a> for Shallow<'a> {
    fn value(top: &Top, data: &'a [u8], ctx: &ReaderContext<'a>) -> Self {
        Self::Value(top.object(data, ctx))
    }

    fn dict(dict: Dict<'a>) -> Self {
        Self::Stream(dict)
    }

    fn stream(stream: Stream<'a>) -> Self {
        Self::Stream(stream.dict().clone())
    }
}

trait Made<'a> {
    /// An object that is no stream, parsed as `top` in `data`, read in `ctx`.
    fn value(top: &Top, data: &'a [u8], ctx: &ReaderContext<'a>) -> Self;
    /// A stream's dictionary, taken without the data.
    fn dict(dict: Dict<'a>) -> Self;
    /// A stream.
    fn stream(stream: Stream<'a>) -> Self;
}

impl<'a> Made<'a> for Object<'a> {
    fn value(top: &Top, data: &'a [u8], ctx: &ReaderContext<'a>) -> Self {
        top.object(data, ctx)
    }

    fn dict(dict: Dict<'a>) -> Self {
        Self::Dict(dict)
    }

    fn stream(stream: Stream<'a>) -> Self {
        Self::Stream(stream)
    }
}

impl<'a> Made<'a> for Written<'a> {
    fn value(top: &Top, data: &'a [u8], ctx: &ReaderContext<'a>) -> Self {
        top.written(data, ctx)
    }

    fn dict(dict: Dict<'a>) -> Self {
        Self::Dict(dict)
    }

    fn stream(stream: Stream<'a>) -> Self {
        Self::Stream(stream)
    }
}

pub(crate) fn find_last_xref_pos(data: &[u8]) -> Option<usize> {
    let needle = b"startxref";
    let found = findr_needle(data, needle);
    crate::work::told(data.len() - found.unwrap_or(0));
    let pos = found?;
    let mut finder = Reader::new(data);
    finder.jump(pos);
    finder.forward_tag(needle)?;
    finder.skip_white_spaces_and_comments();
    finder.read_without_context::<i32>()?.try_into().ok()
}

/// A type of xref entry.
#[derive(Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Clone, Copy)]
enum EntryType {
    /// An indirect object that is at a specific offset in the original data.
    Normal(usize),
    /// An indirect object that is part of an object stream. First number indicates the object
    /// number of the _object stream_ (the generation number is always 0), the second number indicates
    /// the index in the object stream.
    ObjStream(u32, u32),
    /// An object of a rebuilt table at an index, the second number, of the object
    /// stream a rebuild found, the first number its index in [`Repairs::streams`]:
    /// the stream is the one there, whatever entry the table holds for its number.
    Found(u32, u32),
}

type XrefMap = hash::Map<ObjectIdentifier, EntryType>;

#[derive(Debug, Copy, Clone)]
pub(crate) struct TrailerData {
    pub(crate) root_ref: ObjectIdentifier,
    pub(crate) version: Option<PdfVersion>,
}

impl TrailerData {
    pub(crate) fn dummy() -> Self {
        Self {
            root_ref: ObjectIdentifier::new(0, 0),
            version: None,
        }
    }
}

#[derive(Debug, Clone)]
struct SomeRepr {
    document: Option<DocumentId>,
    data: Arc<Data>,
    /// Where the objects of the file start, and `endstream` is ([`Layout`]).
    layout: Layout,
    /// Never changed once read: what is read through it is read from one table.
    map: Arc<XrefMap>,
    /// What a rebuild found besides the entries: the objects the table lists that
    /// cannot be read, the places whose headers name other objects than the entries
    /// leading there, and the object streams its entries into object streams name
    /// ([`EntryType::Found`]).
    repairs: Arc<Repairs>,
    /// What the body at each place an entry leads to holds, read once for every
    /// read of an entry leading there, its arrays and dictionaries
    /// [`body::PARSES`] times at most ([`body::Shapes`]).
    shapes: Arc<Mutex<body::Shapes>>,
    metadata: Arc<Metadata>,
    decryptor: Arc<Decryptor>,
    has_ocgs: bool,
    trailer_data: TrailerData,
}

#[derive(Debug, Clone)]
enum Inner {
    /// A dummy xref table that doesn't have any entries.
    Dummy,
    /// A proper xref table.
    Some(Arc<SomeRepr>),
}

#[derive(Debug)]
struct XRefEntry {
    offset: usize,
    gen_number: i32,
    used: bool,
}

impl XRefEntry {
    pub(crate) fn read(data: &[u8]) -> Option<Self> {
        crate::work::told(XREF_ENTRY_LEN);
        #[inline(always)]
        fn parse_u32(data: &[u8]) -> Option<u32> {
            let mut accum = 0_u32;

            for byte in data {
                accum = accum.checked_mul(10)?;

                match *byte {
                    b'0'..=b'9' => accum = accum.checked_add((*byte - b'0') as u32)?,
                    _ => return None,
                }
            }

            Some(accum)
        }

        let offset = parse_u32(&data[0..10])? as usize;
        let gen_number = i32::try_from(parse_u32(&data[11..16])?).ok()?;

        let used = data[17] == b'n';

        Some(Self {
            offset,
            gen_number,
            used,
        })
    }
}

/// Reads the chain of cross-reference sections `startxref` names, at `pos`, into
/// `xref_map`, and returns the dictionary of the first: its trailer, or its stream's.
/// The entries of a section are inserted after those of the section its /Prev names
/// and then its `/XRefStm`, with theirs, so that an object's entry in a later section
/// replaces one in an earlier. `None` where a section cannot be read, which rebuilds
/// the table.
///
/// Each section is read on the bytes from its offset up to the first that another
/// section of the chain was read on ([`Sections`]): so the chain reads disjoint bytes,
/// each once, and takes time linear in the file, whatever the number of sections and
/// however they nest in each other's strings or streams. A section named at an offset
/// another was read on, as a section named again is, cannot be read: the sections of
/// a sound chain do not overlap.
fn populate_xref_impl<'a>(data: &'a [u8], pos: usize, xref_map: &mut XrefMap) -> Option<&'a [u8]> {
    let mut sections = Sections::default();
    let first = sections.read(data, pos)?;
    let trailer = first.dict_data();
    // The sections read whose entries are not inserted yet, each with the offsets of
    // the sections to read before them, the next last.
    let mut open = vec![(first.before(), first)];
    while let Some((before, _)) = open.last_mut() {
        if let Some(next) = before.pop() {
            let section = sections.read(data, next)?;
            open.push((section.before(), section));
        } else {
            let (_, section) = open.pop()?;
            section.insert(xref_map)?;
        }
    }
    Some(trailer)
}

/// Where the sections of a chain read so far were read: the start and end of the
/// bytes each was read on.
#[derive(Default)]
struct Sections(BTreeMap<usize, usize>);

impl Sections {
    /// The section at `pos` of `data`, read on the bytes up to the first that another
    /// section was read on; `None` where it cannot be read there, or `pos` is in the
    /// bytes another was read on.
    fn read<'a>(&mut self, data: &'a [u8], pos: usize) -> Option<Section<'a>> {
        if self
            .0
            .range(..=pos)
            .next_back()
            .is_some_and(|(_, end)| *end > pos)
        {
            warn!("xref section at offset {pos} is inside another of the chain");

            return None;
        }
        let limit = self.0.range(pos..).next().map_or(data.len(), |(at, _)| *at);
        let mut reader = Reader::new(&data[..limit]);
        reader.jump(pos);
        // In case the position points to before the object number of a xref stream.
        reader.skip_white_spaces_and_comments();

        let (section, end) = if reader
            .clone()
            .read_without_context::<ObjectIdentifier>()
            .is_some()
        {
            let stream = reader
                .read_with_context::<IndirectObject<Stream<'_>>>(&ReaderContext::dummy())?
                .get();
            (Section::Stream(stream), reader.offset())
        } else {
            let mut after = reader.clone();
            let trailer = read_xref_table_trailer(&mut after, &ReaderContext::dummy())?;
            reader.skip_white_spaces();
            reader.forward_tag(b"xref")?;
            reader.skip_white_spaces();
            (Section::Table(reader, trailer), after.offset())
        };
        self.0.insert(pos, end);
        Some(section)
    }
}

/// A cross-reference section read, whose entries are still to be inserted.
enum Section<'a> {
    /// A table: a reader at its first subsection, and its trailer.
    Table(Reader<'a>, Dict<'a>),
    /// A cross-reference stream.
    Stream(Stream<'a>),
}

impl<'a> Section<'a> {
    /// The trailer, or the stream's dictionary.
    fn dict_data(&self) -> &'a [u8] {
        match self {
            Self::Table(_, trailer) => trailer.data(),
            Self::Stream(stream) => stream.dict().data(),
        }
    }

    /// The offsets of the sections whose entries are inserted before this one's, the
    /// first last: the one /Prev names, then, for a table of a hybrid file, the
    /// stream `/XRefStm` names, whose entries take precedence.
    fn before(&self) -> Vec<usize> {
        let offsets = match self {
            Self::Table(_, trailer) => [trailer.get::<i32>(XREF_STM), trailer.get::<i32>(PREV)],
            Self::Stream(stream) => [None, stream.dict().get::<i32>(PREV)],
        };
        offsets
            .into_iter()
            .flatten()
            .map(|at| at as usize)
            .collect()
    }

    /// Inserts the section's entries into `insert_map`.
    fn insert(self, insert_map: &mut XrefMap) -> Option<()> {
        match self {
            Self::Table(reader, _) => populate_from_xref_table(reader, insert_map),
            Self::Stream(stream) => populate_from_xref_stream(&stream, insert_map),
        }
    }
}

pub(super) struct SubsectionHeader {
    pub(super) start: u32,
    pub(super) num_entries: u32,
}

impl Readable<'_> for SubsectionHeader {
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        r.skip_white_spaces();
        let start = r.read_without_context::<u32>()?;
        r.skip_white_spaces();
        let num_entries = r.read_without_context::<u32>()?;
        r.skip_white_spaces();

        Some(Self { start, num_entries })
    }
}

/// Inserts the entries of the table whose first subsection `reader` is at.
fn populate_from_xref_table(mut reader: Reader<'_>, insert_map: &mut XrefMap) -> Option<()> {
    let mut max_obj = 0;

    while let Some(header) = reader.read_without_context::<SubsectionHeader>() {
        reader.skip_white_spaces();

        let start = header.start;
        let end = start.checked_add(header.num_entries)?;
        // Room for the entries the subsection lists, as many as the bytes can hold.
        let room = (reader.len() - reader.offset()) / XREF_ENTRY_LEN;
        hash::reserve(insert_map, room.min(header.num_entries as usize));

        for obj_number in start..end {
            max_obj = max(max_obj, obj_number);
            let bytes = reader.read_bytes(XREF_ENTRY_LEN)?;
            let entry = XRefEntry::read(bytes)?;

            // Specification says we should ignore any object number > SIZE, but probably
            // not important?
            if entry.used {
                insert_map.insert(
                    ObjectIdentifier::new(obj_number as i32, entry.gen_number),
                    EntryType::Normal(entry.offset),
                );
            }
        }
    }

    Some(())
}

/// Inserts the entries of cross-reference stream `stream`.
fn populate_from_xref_stream(stream: &Stream<'_>, insert_map: &mut XrefMap) -> Option<()> {
    let size = stream.dict().get::<u32>(SIZE)?;

    let [f1_len, f2_len, f3_len] = stream.dict().get::<[u8; 3]>(W)?;

    if f2_len > size_of::<u64>() as u8 {
        error!("xref offset length is larger than the allowed limit");

        return None;
    }

    // Do such files exist?
    if f1_len != 1 {
        warn!("first field in xref stream was longer than 1");
    }

    let xref_data = stream.decoded().ok()?;
    let mut xref_reader = Reader::new(xref_data.as_ref());

    if let Some(arr) = stream.dict().get::<Array<'_>>(INDEX) {
        let mut pairs = arr.flex_iter();

        while let Some((start, num_elements)) = pairs.next::<(u32, u32)>() {
            xref_stream_subsection(
                &mut xref_reader,
                start,
                num_elements,
                f1_len,
                f2_len,
                f3_len,
                insert_map,
            )?;
        }
    } else {
        xref_stream_subsection(
            &mut xref_reader,
            0,
            size,
            f1_len,
            f2_len,
            f3_len,
            insert_map,
        )?;
    }

    Some(())
}

fn xref_stream_num(data: &[u8]) -> Option<u32> {
    Some(match data.len() {
        0 => return None,
        1 => u8::from_be(data[0]) as u32,
        2 => u16::from_be_bytes(data[0..2].try_into().ok()?) as u32,
        3 => u32::from_be_bytes([0, data[0], data[1], data[2]]),
        4 => u32::from_be_bytes(data[0..4].try_into().ok()?),
        8 => {
            if let Ok(num) = u32::try_from(u64::from_be_bytes(data[0..8].try_into().ok()?)) {
                return Some(num);
            } else {
                warn!("xref stream number is too large");

                return None;
            }
        }
        _n => {
            warn!("invalid xref stream number {_n}");

            return None;
        }
    })
}

fn xref_stream_subsection<'a>(
    xref_reader: &mut Reader<'a>,
    start: u32,
    num_elements: u32,
    f1_len: u8,
    f2_len: u8,
    f3_len: u8,
    insert_map: &mut XrefMap,
) -> Option<()> {
    let width = usize::from(f1_len) + usize::from(f2_len) + usize::from(f3_len);
    if width == 0 {
        // Entries of no bytes, as many as the dictionary says, tell nothing.
        warn!("xref stream entries have no width");

        return None;
    }
    for i in 0..num_elements {
        crate::work::told(width);
        let f_type = if f1_len == 0 {
            1
        } else {
            // We assume a length of 1.
            xref_reader.read_bytes(1)?[0]
        };

        let obj_number = start.checked_add(i)?;

        match f_type {
            // We don't care about free objects.
            0 => {
                xref_reader.skip_bytes(f2_len as usize + f3_len as usize)?;
            }
            1 => {
                let offset = if f2_len > 0 {
                    let data = xref_reader.read_bytes(f2_len as usize)?;
                    xref_stream_num(data)?
                } else {
                    0
                };

                let gen_number = if f3_len > 0 {
                    let data = xref_reader.read_bytes(f3_len as usize)?;
                    xref_stream_num(data)?
                } else {
                    0
                };

                insert_map.insert(
                    ObjectIdentifier::new(obj_number as i32, gen_number as i32),
                    EntryType::Normal(offset as usize),
                );
            }
            2 => {
                let obj_stream_number = {
                    let data = xref_reader.read_bytes(f2_len as usize)?;
                    xref_stream_num(data)?
                };
                let gen_number = 0;
                let index = if f3_len > 0 {
                    let data = xref_reader.read_bytes(f3_len as usize)?;
                    xref_stream_num(data)?
                } else {
                    0
                };

                insert_map.insert(
                    ObjectIdentifier::new(obj_number as i32, gen_number),
                    EntryType::ObjStream(obj_stream_number, index),
                );
            }
            _ => {
                warn!("xref has unknown field type {f_type}");

                return None;
            }
        }
    }

    Some(())
}

fn read_xref_table_trailer<'a>(
    reader: &mut Reader<'a>,
    ctx: &ReaderContext<'a>,
) -> Option<Dict<'a>> {
    reader.skip_white_spaces();
    reader.forward_tag(b"xref")?;
    reader.skip_white_spaces();

    while let Some(header) = reader.read_without_context::<SubsectionHeader>() {
        let len = XREF_ENTRY_LEN.checked_mul(header.num_entries as usize)?;
        reader.jump(reader.offset().checked_add(len)?);
    }

    reader.skip_white_spaces();
    reader.forward_tag(b"trailer")?;
    reader.skip_white_spaces();

    reader.read_with_context::<Dict<'_>>(ctx)
}

fn get_decryptor(trailer_dict: &Dict<'_>, password: &[u8]) -> Result<Decryptor, XRefError> {
    if let Some(encryption_dict) = trailer_dict.get::<Dict<'_>>(ENCRYPT) {
        let id = if let Some(id) = trailer_dict
            .get::<Array<'_>>(ID)
            .and_then(|a| a.flex_iter().next::<object::String<'_>>())
        {
            id.to_vec()
        } else {
            // Assume an empty ID entry.
            vec![]
        };

        get(&encryption_dict, &id, password).map_err(XRefError::Encryption)
    } else {
        Ok(Decryptor::None)
    }
}

/// An object stream's decoded data and where each object in it starts, read once
/// however many of its objects are read.
pub(crate) struct ObjectStream {
    data: Vec<u8>,
    /// Whether the data are cut where the stream's go on ([`Taken::Contained`]),
    /// so that their end is no end of an object.
    cut: bool,
    /// Each object's number and offset in `data`.
    offsets: Vec<(u32, usize)>,
    /// The offsets, each once, in order: where the slot of each object ends
    /// ([`Place`]).
    starts: Vec<usize>,
    /// The index of the last object of each number, built the first time an entry's
    /// index names another object.
    by_number: OnceLock<hash::Map<u32, usize>>,
}

impl ObjectStream {
    /// The object stream `inner`, whose data are `cut` where the stream's go on: a
    /// number of its header that reaches the end of cut data may go on past it, so
    /// the header cannot be read.
    pub(crate) fn new(inner: &Stream<'_>, cut: bool) -> Option<Self> {
        let num_objects = inner.dict().get::<usize>(N)?;
        let first_offset = inner.dict().get::<usize>(FIRST)?;
        let data = inner.decoded().ok()?.into_owned();

        let mut r = Reader::new(&data);
        let number = |r: &mut Reader<'_>| {
            r.skip_white_spaces_and_comments();
            let number = r.read_without_context::<usize>()?;
            (!cut || r.offset() < data.len()).then_some(number)
        };

        let mut offsets = vec![];

        for _ in 0..num_objects {
            let obj_num = u32::try_from(number(&mut r)?).ok()?;
            let relative_offset = number(&mut r)?;
            offsets.push((obj_num, first_offset.checked_add(relative_offset)?));
        }
        crate::work::read_bodies(data.len());
        crate::work::object_stream_entries(offsets.len());
        let mut starts: Vec<usize> = offsets.iter().map(|(_, at)| *at).collect();
        starts.sort_unstable();
        starts.dedup();

        Some(Self {
            data,
            cut,
            offsets,
            starts,
            by_number: OnceLock::new(),
        })
    }

    /// The slot of the object at `at`: up to the next offset the header lists, or the
    /// end of the data, which goes on unknown where they are cut; and where the slot
    /// after it ends.
    fn slot(&self, at: usize) -> Slot {
        let next = self.starts.partition_point(|start| *start <= at);
        let end = |next: usize| {
            self.starts
                .get(next)
                .map_or(self.data.len(), |next| *next)
                .min(self.data.len())
        };
        Slot {
            end: end(next),
            open: self.cut && end(next) == self.data.len(),
            next: end(next + 1),
        }
    }

    /// The decoded data.
    pub(crate) fn data(&self) -> &[u8] {
        &self.data
    }

    /// Where in the data object `id` starts, which an entry puts at `index`: the
    /// stream's header names the number of each object it holds, and an index that
    /// names another object is damaged, so the object of `id`'s number is read
    /// wherever it is in the stream, or none where the stream holds none.
    fn offset(&self, id: ObjectIdentifier, index: u32) -> Option<usize> {
        let number = u32::try_from(id.obj_number).ok()?;
        match self.offsets.get(index as usize) {
            Some(&(listed, offset)) if listed == number => Some(offset),
            _ => {
                let by_number = self.by_number.get_or_init(|| {
                    let numbers = self.offsets.iter().enumerate();
                    numbers
                        .map(|(index, (number, _))| (*number, index))
                        .collect()
                });
                Some(self.offsets[*by_number.get(&number)?].1)
            }
        }
    }
}

fn parse_metadata(info_dict: &Dict<'_>) -> Metadata {
    Metadata {
        creation_date: info_dict
            .get::<object::String<'_>>(CREATION_DATE)
            .and_then(|c| DateTime::from_bytes(&c)),
        modification_date: info_dict
            .get::<object::String<'_>>(MOD_DATE)
            .and_then(|c| DateTime::from_bytes(&c)),
        title: info_dict
            .get::<object::String<'_>>(TITLE)
            .map(|t| Box::from(t.as_bytes())),
        author: info_dict
            .get::<object::String<'_>>(AUTHOR)
            .map(|t| Box::from(t.as_bytes())),
        subject: info_dict
            .get::<object::String<'_>>(SUBJECT)
            .map(|t| Box::from(t.as_bytes())),
        keywords: info_dict
            .get::<object::String<'_>>(KEYWORDS)
            .map(|t| Box::from(t.as_bytes())),
        creator: info_dict
            .get::<object::String<'_>>(CREATOR)
            .map(|t| Box::from(t.as_bytes())),
        producer: info_dict
            .get::<object::String<'_>>(PRODUCER)
            .map(|t| Box::from(t.as_bytes())),
    }
}

#[cfg(test)]
#[allow(dead_code, reason = "the tests here use some of the files")]
#[path = "../tests/common/rc4.rs"]
mod rc4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circular_prev_chain() {
        let mut pdf = b"%PDF-1.0\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec();
        let expected_xref_pos = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n\
                 0 1\n\
                 0000000000 65535 f \r\n\
                 trailer\n<< /Size 1 /Root 1 0 R /Prev {expected_xref_pos} >>\n\
                 startxref\n{expected_xref_pos}\n%%EOF"
            )
            .as_bytes(),
        );

        let mut xref_map = hash::Map::default();
        let xref_pos = find_last_xref_pos(pdf.as_ref()).unwrap();
        let _result = populate_xref_impl(pdf.as_ref(), xref_pos, &mut xref_map);
    }

    /// A file of a catalog, a page tree and a 100 × 100 page, then `body`, whose
    /// objects are at `entries`, numbered from 4, offsets in `body`; with
    /// `startxref` at offset 9 where `broken`, which rebuilds the table.
    fn file_with(body: &[u8], entries: &[usize], broken: bool) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![];
        for object in [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        ] {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{object}\nendobj\n", offsets.len()).bytes());
        }
        offsets.extend(entries.iter().map(|at| out.len() + at));
        out.extend(body);
        out.push(b'\n');
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).bytes());
        for offset in &offsets {
            out.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        let start = if broken { 9 } else { xref };
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
                offsets.len() + 1
            )
            .bytes(),
        );
        out
    }

    /// Object 4, then a second object 4 of `n` numbers that entries 5 to `n` + 4
    /// lead to.
    fn aliases(n: usize, broken: bool) -> Vec<u8> {
        let body = format!(
            "4 0 obj\nnull\nendobj\n4 0 obj\n[{}]\nendobj\n",
            " 0".repeat(n)
        );
        let second = body.rfind("4 0 obj").unwrap();
        let mut entries = vec![0];
        entries.extend(iter::repeat_n(second, n));
        file_with(body.as_bytes(), &entries, broken)
    }

    /// Objects 4 to `n` + 3, each in a string of the one before; their entries lead to
    /// their headers. With `space`, a header follows white space.
    fn nested(n: usize, space: bool, broken: bool) -> Vec<u8> {
        let space = if space { " " } else { "" };
        let mut body = String::new();
        for k in (4..n + 4).rev() {
            body = format!("{k} 0 obj ({space}{body}{space}) endobj");
        }
        let entries: Vec<usize> = (4..n + 4)
            .map(|k| body.find(&format!("{k} 0 obj")).unwrap())
            .collect();
        file_with(body.as_bytes(), &entries, broken)
    }

    /// Object 4 holding `n` times `unit`, in a file whose table is rebuilt.
    fn repeated(unit: &str, n: usize) -> Vec<u8> {
        around(&unit.repeat(n))
    }

    /// Object 4 holding `body`, in a file whose table is rebuilt.
    fn around(body: &str) -> Vec<u8> {
        file_with(format!("4 0 obj\n{body}\nendobj\n").as_bytes(), &[0], true)
    }

    /// The work opening `file` takes, the bytes lexed, those worked on otherwise
    /// ([`crate::work::TOLD`]) and the scan's other work, after checking it opens to
    /// the 100 × 100 page.
    fn work(file: &[u8]) -> usize {
        OPEN_BYTES.set(0);
        crate::work::EXAMINED.set(0);
        crate::work::TOLD.set(0);
        let pdf = crate::Pdf::new(file.to_vec()).unwrap();
        let read = OPEN_BYTES.get() + crate::work::EXAMINED.get() + crate::work::TOLD.get();
        let pages = pdf.pages();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].media_box().x1, 100.0);
        read
    }

    // Opening a file reads each part of it a bounded number of times: to check the
    // table, however many entries lead to one object or into objects that hold
    // others, and to scan a file whose table is rebuilt, however many headers,
    // strings, comments and dictionaries its objects hold, however they nest, and
    // whether or not they end. Each family's work doubles with the file.
    #[test]
    fn opening_a_file_reads_it_in_linear_time() {
        type Family = (&'static str, fn(usize) -> Vec<u8>);
        let families: [Family; 37] = [
            // Entries at each `0` of `0…09 0 obj`: each reads a header in its slot of
            // one byte, which the next `0` ends, not on to `obj`.
            ("entries in the zeros of a header", |n| {
                let entries: Vec<usize> = (0..n).collect();
                let body = format!("{}9 0 obj\nnull\nendobj\n", "0".repeat(n));
                file_with(body.as_bytes(), &entries, false)
            }),
            // Entries every 8 bytes of one number of `8 n` digits: each reads the
            // number in its slot, not to the end of the run.
            ("entries in a long number", |n| {
                let entries: Vec<usize> = (0..n).map(|i| 8 * i).collect();
                let body = format!("{}\n9 0 obj\nnull\nendobj\n", "1".repeat(8 * n));
                file_with(body.as_bytes(), &entries, false)
            }),
            // Entries at headers in one comment line, each before a `%`, then `8 n`
            // spaces and the object: from each header, a comment runs past its slot,
            // so the rebuild does not look for an object past the line and spaces.
            ("entries in a comment line before white space", |n| {
                let body = format!(
                    "4 0 obj %{}\n{}null\nendobj\n",
                    " 5 0 obj%".repeat(n),
                    " ".repeat(8 * n)
                );
                let entries: Vec<usize> =
                    body.match_indices("0 obj").map(|(at, _)| at - 2).collect();
                file_with(body.as_bytes(), &entries, false)
            }),
            ("aliases", |n| aliases(n, false)),
            ("nested", |n| nested(n, false, false)),
            ("nested after space", |n| nested(n, true, false)),
            ("aliases, rebuilt", |n| aliases(n, true)),
            ("nested, rebuilt", |n| nested(n, false, true)),
            ("nested after space, rebuilt", |n| nested(n, true, true)),
            ("unterminated strings", |n| repeated("( 5 0 obj ", n)),
            ("comments", |n| repeated("1 %x ", n)),
            ("headers split by comments", |n| repeated("9 %x\n0 obj ", n)),
            // Each probe of a dictionary reads to the end of the file (#53).
            ("dictionaries over unterminated strings", |n| {
                repeated("<< /A ( ", n)
            }),
            ("nested dictionaries around an unterminated string", |n| {
                around(&format!("{}(", "<< /A ".repeat(n)))
            }),
            ("nested dictionaries", |n| {
                around(&format!("{}{}", "<< /A ".repeat(n), ">> ".repeat(n)))
            }),
            ("headers in nested arrays", |n| repeated("5 0 obj [ ", n)),
            // Objects after one whose string never ends, each holding a header in a
            // string: each is found.
            ("objects after an unterminated string", |n| {
                let objects = "5 0 obj << /T (9 0 obj) >> endobj ".repeat(n);
                around(&format!("( endobj {objects}"))
            }),
            // Strings opened at escaped parentheses end with the string around them,
            // where readings from each header meet, before a long tail.
            ("headers in strings that end together", |n| {
                let tail = "/x ".repeat(n);
                around(&format!("[ ( {}) {tail}]", "5 0 obj [ \\( ".repeat(n)))
            }),
            // So too where they end in a comment line, before its end.
            ("headers in strings that end in a comment", |n| {
                let tail = "/x ".repeat(n);
                around(&format!("[ x% {}) \n{tail}]", "5 0 obj [ \\( ".repeat(n)))
            }),
            // Readings started in a comment line, open where it ends.
            ("headers in a comment line", |n| {
                let tail = "/x ".repeat(n);
                around(&format!(
                    "[ x% {}\n{tail}{}",
                    "5 0 obj [ ".repeat(50),
                    "] ".repeat(51)
                ))
            }),
            // Strings nested in each other end one after another in a line, each
            // before a different `%` of it: each reading leaves its string there and
            // reads a comment to the end of the line.
            ("comments entered at each string end of a line", |n| {
                around(&format!(
                    "{}{}\n",
                    "5 0 obj [ ( ".repeat(n),
                    "% ) ".repeat(n)
                ))
            }),
            // A catalog holding a dictionary of names that hash alike with a hash of
            // fixed keys: 16 bytes, the first 8 of which cancel the seed of
            // rustc-hash's byte hashing. With such a hash, each name inserted into
            // the dictionary's map is compared with every one before it.
            ("names that hash alike", |n| {
                let mut catalog = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Test << ".to_vec();
                for i in 0..n {
                    catalog.push(b'/');
                    catalog.extend(0x243f_6a88_85a3_08d3_u64.to_le_bytes());
                    catalog.extend(format!("{i:08x} 0 ").bytes());
                }
                catalog.extend(b">> >>\nendobj\n");
                file_with(&catalog, &[], true)
            }),
            // Streams that are headers' objects, whose data no `endstream` ends: the
            // first search for one reads to the end of the file, once for them all.
            ("streams that do not end", |n| {
                repeated("5 0 obj << /Length 1 >> stream\n", n)
            }),
            // Streams whose /Length leads into one long run of white space, with no
            // `endstream` after it: each reads a bounded part of the run.
            ("streams whose length leads into white space", |n| {
                let unit = "5 0 obj << /Length 0000000000 >> stream\n";
                let length = unit.len() * n;
                let units = unit
                    .replace("0000000000", &format!("{length:010}"))
                    .repeat(n);
                around(&format!("{units}{}", " ".repeat(length + 100)))
            }),
            // Streams whose /Length leads to `endobj`, the data's `endstream` missing,
            // before one `endstream` after them all: the search for it reads to it
            // once for them all.
            ("streams ended by endobj before one endstream", |n| {
                around(&format!(
                    "{}endstream",
                    "5 0 obj << /Length 0 >> stream\nendobj\n".repeat(n)
                ))
            }),
            // Streams, each holding a header, whose /Length is a reference and whose
            // data end at an `endstream`: each is read past, the header unread.
            ("streams holding headers", |n| {
                repeated(
                    "5 0 obj << /Length 9 0 R >> stream\n6 0 obj << >> endobj\nendstream ",
                    n,
                )
            }),
            // Streams holding the keyword `stream`, whose /Length is a reference to an
            // object after each: the file's headers are found once for them all.
            ("streams whose lengths are references", |n| {
                around(
                    &(0..n)
                        .map(|k| {
                            let (stream, length) = (2 * k + 10, 2 * k + 11);
                            format!(
                                "{stream} 0 obj << /Length {length} 0 R >> stream\nx stream x\n\
                                 endstream endobj\n{length} 0 obj 10 endobj\n"
                            )
                        })
                        .collect::<String>(),
                )
            }),
            // Streams holding the keyword `stream`, whose /Length is a reference to one
            // object, a long number after them all: it is read once for them all.
            ("streams sharing one length object", |n| {
                let streams =
                    "5 0 obj << /Length 9 0 R >> stream\nx stream x\nendstream endobj\n".repeat(n);
                around(&format!("{streams}9 0 obj {}10 endobj", "0".repeat(n)))
            }),
            // Streams holding the keyword `stream` and the last header of the object
            // their /Length refers to.
            ("streams holding their length object", |n| {
                repeated(
                    "5 0 obj << /Length 9 0 R >> stream\n9 0 obj 1 endobj stream\nendstream endobj\n",
                    n,
                )
            }),
            // Streams holding the keyword `stream`, each /Length a reference to another
            // object, whose headers are in one comment line after them all, each before
            // a `%`: no object after a header is read past the white space after it.
            ("streams whose length objects are in a comment line", |n| {
                let streams: String = (0..n)
                    .map(|k| {
                        format!(
                            "{} 0 obj << /Length {} 0 R >> stream\nx stream x\nendstream endobj\n",
                            2 * k + 10,
                            2 * k + 11
                        )
                    })
                    .collect();
                let headers: String = (0..n).map(|k| format!(" {} 0 obj %", 2 * k + 11)).collect();
                around(&format!("{streams}%{headers}\n0\n"))
            }),
            // Streams holding the keyword `stream`, whose /Length is a reference to an
            // object of which each of as many headers after them all leads to no
            // `endstream`: each stream tries a bounded number of them.
            ("streams with many headers of their length object", |n| {
                let streams =
                    "5 0 obj << /Length 9 0 R >> stream\nx stream x\nendstream endobj\n".repeat(n);
                around(&format!("{streams}{}", "9 0 obj 1 endobj\n".repeat(n)))
            }),
            // Streams holding the keyword `stream`, whose /Length is a reference to one
            // object before them all.
            ("streams sharing one length object before them", |n| {
                let streams =
                    "5 0 obj << /Length 9 0 R >> stream\nx stream x\nendstream endobj\n".repeat(n);
                around(&format!("9 0 obj 10 endobj\n{streams}"))
            }),
            // Streams holding the keyword `stream`, each /Length a reference to another
            // object after them all, a comment before its value.
            ("streams whose length objects hold a comment", |n| {
                let streams: String = (0..n)
                    .map(|k| {
                        format!(
                            "{} 0 obj << /Length {} 0 R >> stream\nx stream x\nendstream endobj\n",
                            2 * k + 10,
                            2 * k + 11
                        )
                    })
                    .collect();
                let objects: String = (0..n)
                    .map(|k| format!("{} 0 obj % the length\n10 endobj\n", 2 * k + 11))
                    .collect();
                around(&format!("{streams}{objects}"))
            }),
            // Streams each in the data of the one before, ended by as many `endstream`
            // after them all, each /Length leading to its own, the last.
            ("streams whose data nest", |n| {
                let mut body = String::new();
                let mut starts = vec![];
                for k in 0..n {
                    body.push_str(&format!(
                        "{} 0 obj << /Length {} 0 R >> stream\n",
                        k + 10,
                        k + n + 10
                    ));
                    starts.push(body.len());
                }
                let mut ends = vec![];
                for _ in 0..n {
                    ends.push(body.len());
                    body.push_str("endstream\n");
                }
                for k in 0..n {
                    body.push_str(&format!(
                        "{} 0 obj {} endobj\n",
                        k + n + 10,
                        ends[n - 1 - k] - starts[k]
                    ));
                }
                around(&body)
            }),
            // Streams holding the keyword `stream` and a header of the object their
            // /Length refers to, whose value leads past the next three streams, before
            // a last header of it, leading to no `endstream`: the data each /Length
            // would end overlap the next three streams', and each is read.
            ("streams whose lengths lead past the next ones", |n| {
                let unit = |length: usize| {
                    format!(
                        "5 0 obj << /Length 9 0 R >> stream\n9 0 obj {length:08} endobj stream\nendstream endobj\n"
                    )
                };
                let size = unit(0).len();
                let data = size
                    - "5 0 obj << /Length 9 0 R >> stream\n".len()
                    - "endstream endobj\n".len();
                around(&format!(
                    "{}9 0 obj 1 endobj",
                    unit(3 * size + data).repeat(n)
                ))
            }),
            // Streams that no `endstream` ends, before a later stream's: the data of
            // each are read, the searches for both keywords reading each part once.
            ("streams ended by a later stream's endstream", |n| {
                around(&format!(
                    "{}6 0 obj << >> stream\nx\nendstream",
                    "5 0 obj << /Length 9 0 R >> stream\nx\nendobj\n".repeat(n)
                ))
            }),
            // A trailer names a large catalog many times.
            ("trailers naming a large catalog", |n| {
                let catalog = format!(
                    "1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Pad [{}] >>\nendobj\n",
                    " 0".repeat(n)
                );
                let trailers = "trailer << /Root 1 0 R >> ".repeat(n);
                file_with(format!("{catalog}{trailers}").as_bytes(), &[], true)
            }),
        ];
        for (family, file) in families {
            // A dictionary of more than 8 keys is kept sorted, which compares each key
            // with at least the next in order, each time the dictionary is parsed.
            let most = if family == "names that hash alike" {
                15
            } else {
                13
            };
            let [small, large] = [1000, 2000].map(|n| {
                let file = file(n);
                let work = work(&file);
                assert!(
                    work <= most * file.len(),
                    "{family}: {work} steps to open {} bytes",
                    file.len()
                );
                work
            });
            // A string or comment costs a lookup logarithmic in the file, and where a
            // string ends among blocks varies with the size.
            assert!(
                large <= small * 25 / 10,
                "{family}: {small} steps for 1000, {large} for 2000"
            );
        }
    }

    /// The objects of a file of a catalog, a page tree and a 100 × 100 page, and the
    /// entries of a cross-reference section listing them, as a table's rows or as a
    /// stream's data with /W [1 4 2].
    fn three_objects() -> (Vec<u8>, String, Vec<u8>) {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut rows = "0000000000 65535 f \n".to_owned();
        let mut packed = vec![0, 0, 0, 0, 0, 255, 255];
        for (i, object) in [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        ]
        .iter()
        .enumerate()
        {
            rows.push_str(&format!("{:010} 00000 n \n", out.len()));
            packed.push(1);
            packed.extend((out.len() as u32).to_be_bytes());
            packed.extend([0, 0]);
            out.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
        }
        (out, rows, packed)
    }

    /// A file of a catalog, a page tree and a 100 × 100 page, whose `n`
    /// cross-reference sections each sit in the one before, chained by /Prev, the last
    /// holding `pad` bytes: tables, each in a string of the trailer before, or, with
    /// `streams`, streams, each in the data of the one before, whose /Length is wrong,
    /// so that each reads its data up to the `endstream` after the rest.
    fn nested_sections(n: usize, pad: usize, streams: bool) -> Vec<u8> {
        let (mut out, rows, packed) = three_objects();
        let first = out.len();
        // Each section's start, of one length: the next's offset written in ten digits.
        let head = |i: usize, next: Option<usize>| -> Vec<u8> {
            let prev = match next {
                Some(at) => format!("/Prev {at:010}"),
                None => format!("/Last {:010}", 0),
            };
            if streams {
                let mut head = format!(
                    "{} 0 obj\n<< /Type /XRef /Size 4 /W [1 4 2] /Root 1 0 R {prev} /Length 0 >>\nstream\n",
                    100 + i
                )
                .into_bytes();
                head.extend(&packed);
                head
            } else {
                format!("xref\n0 4\n{rows}trailer\n<< /Size 4 /Root 1 0 R {prev} /S (").into_bytes()
            }
        };
        let unit = head(0, Some(0)).len();
        for i in 0..n {
            out.extend(head(i, (i + 1 < n).then(|| first + (i + 1) * unit)));
        }
        out.extend(b"x".repeat(pad));
        let tail: &[u8] = if streams {
            b"\nendstream\nendobj\n"
        } else {
            b") >>"
        };
        for _ in 0..n {
            out.extend(tail);
        }
        out.extend(format!("\nstartxref\n{first}\n%%EOF\n").bytes());
        out
    }

    /// [`work`], bounded by the file's length.
    fn bounded(file: &[u8]) -> usize {
        let work = work(file);
        assert!(
            work <= 13 * file.len(),
            "{work} steps to open {} bytes",
            file.len()
        );
        work
    }

    // The sections of a chain are read on disjoint bytes, each once: sections nested
    // in each other's trailer strings or stream data are read once, whatever their
    // number and the size of what they hold, and the table is rebuilt, rather than
    // each read to the end of the nest.
    #[test]
    fn nested_cross_reference_sections_are_read_once() {
        for streams in [false, true] {
            // The size doubles, the number of sections stays.
            let [small, large] =
                [1 << 16, 1 << 17].map(|pad| bounded(&nested_sections(64, pad, streams)));
            assert!(
                large <= small * 25 / 10,
                "streams {streams}: {small} steps for 64 KB, {large} for 128 KB"
            );
            // The number of sections doubles, the size barely grows.
            let [few, many] = [64, 128].map(|n| bounded(&nested_sections(n, 1 << 18, streams)));
            assert!(
                many <= few * 12 / 10,
                "streams {streams}: {few} steps for 64 sections, {many} for 128"
            );
        }
    }

    /// A file of `n` sections chained by /Prev, apart, each listing object 3 at a
    /// page: the last section, which `startxref` names, at a 100 × 100 page, the
    /// others at a 200 × 200 page after it in the file, which a rebuilt table takes.
    fn long_chain(n: usize) -> Vec<u8> {
        let (mut out, rows, _) = three_objects();
        // The rows of objects 1 and 2.
        let rows: String = rows
            .lines()
            .skip(1)
            .take(2)
            .map(|row| format!("{row}\n"))
            .collect();
        let first_page = out.windows(7).position(|w| w == b"3 0 obj").unwrap();
        let other_page = out.len();
        out.extend(b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>\nendobj\n");
        let mut prev = None;
        for i in 0..n {
            let at = out.len();
            let page = if i + 1 == n { first_page } else { other_page };
            out.extend(
                format!(
                    "xref\n0 3\n0000000000 65535 f \n{rows}3 1\n{page:010} 00000 n \n\
                     trailer\n<< /Size 4 /Root 1 0 R"
                )
                .bytes(),
            );
            if let Some(prev) = prev {
                out.extend(format!(" /Prev {prev}").bytes());
            }
            out.extend(b" >>\n");
            prev = Some(at);
        }
        out.extend(format!("startxref\n{}\n%%EOF\n", prev.unwrap()).bytes());
        out
    }

    // A chain of many sections apart is read whole, however long: the number of
    // sections is bounded by the file's length, as they do not overlap.
    #[test]
    fn a_long_chain_of_sections_is_read() {
        let file = long_chain(1000);
        let pdf = crate::Pdf::new(file.clone()).unwrap();
        assert_eq!(pdf.pages()[0].media_box().x1, 100.0);
        bounded(&file);
    }

    // A cross-reference stream whose entries take no bytes, as many as it says,
    // tells nothing: the table is rebuilt, without an entry for each.
    #[test]
    fn a_cross_reference_stream_of_entries_of_no_width_is_rebuilt() {
        let (mut out, _, _) = three_objects();
        out.extend(format!("%{}\n", "x".repeat(4096)).bytes());
        let at = out.len();
        out.extend(
            format!(
                "9 0 obj\n<< /Type /XRef /Size 1000000 /W [0 0 0] /Root 1 0 R /Length 0 >>\n\
                 stream\n\nendstream\nendobj\nstartxref\n{at}\n%%EOF\n"
            )
            .bytes(),
        );
        bounded(&out);
    }

    /// A stream object of `data`, with `dict` written into its dictionary.
    fn stream(dict: &str, data: &str) -> String {
        format!(
            "<< {dict} /Length {} >>\nstream\n{data}\nendstream",
            data.len()
        )
    }

    /// An object stream of `objects`, with `dict` written into its dictionary.
    fn object_stream(dict: &str, objects: &[(u32, &str)]) -> String {
        let mut header = String::new();
        let mut body = String::new();
        for (number, object) in objects {
            header += &format!("{number} {} ", body.len());
            body += object;
            body.push('\n');
        }
        let first = header.len();
        let length = header.len() + body.len();
        format!(
            "<< /Type /ObjStm /N {} /First {first} {dict} >>\nstream\n{header}{body}\nendstream",
            objects.len()
        )
        .replace("/Length LEN", &format!("/Length {length}"))
    }

    /// A file of `objects` whose table is an xref stream listing them and the
    /// `compressed` objects, each by number, object stream and index, with catalog 1.
    fn file_with_stream_table(
        objects: &[(u32, String)],
        compressed: &[(u32, u32, u32)],
    ) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut entries = hash::Map::default();
        for (number, object) in objects {
            entries.insert(*number, (1_u8, out.len() as u32, 0_u16));
            out.extend(format!("{number} 0 obj\n{object}\nendobj\n").bytes());
        }
        for (number, stream, index) in compressed {
            entries.insert(*number, (2, *stream, *index as u16));
        }
        let table = 1 + entries
            .keys()
            .chain(objects.iter().map(|(n, _)| n))
            .max()
            .unwrap();
        let offset = out.len() as u32;
        entries.insert(table, (1, offset, 0));
        let mut data = vec![];
        for number in 0..=table {
            let (kind, field, index) = entries.get(&number).copied().unwrap_or((0, 0, 0));
            data.push(kind);
            data.extend(field.to_be_bytes());
            data.extend(index.to_be_bytes());
        }
        out.extend(
            format!(
                "{table} 0 obj\n<< /Type /XRef /Size {} /W [1 4 2] /Root 1 0 R /Length {} >>\nstream\n",
                table + 1,
                data.len()
            )
            .bytes(),
        );
        out.extend(data);
        out.extend(format!("\nendstream\nendobj\nstartxref\n{offset}\n%%EOF\n").bytes());
        out
    }

    /// How many object streams `xref` keeps decoded.
    fn kept_streams(xref: &XRef) -> usize {
        match &xref.0 {
            Inner::Some(repr) => repr.data.kept_streams(),
            Inner::Dummy => 0,
        }
    }

    /// A page whose image, object 4, names its `/DecodeParms`, object 5, in an object
    /// stream, object 6, whose dictionary is `objstm_dict`; and `filler` bytes of a
    /// stream no page uses, object 7.
    fn image_with_parameters_in_object_stream(objstm_dict: &str, filler: usize) -> Vec<u8> {
        let image = "<< /Filter /ASCIIHexDecode /DecodeParms 5 0 R /Length 7 >>\nstream\n616263>\nendstream";
        let objstm = "5 0 << /Predictor 1 >>";
        let objects = [
            (1, CATALOG.to_owned()),
            (2, TREE.to_owned()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>".to_owned(),
            ),
            (4, image.to_owned()),
            (
                6,
                format!(
                    "<< /Type /ObjStm /N 1 /First 4 {} >>\nstream\n{objstm}\nendstream",
                    objstm_dict.replace("{len}", &objstm.len().to_string())
                ),
            ),
            (
                7,
                format!(
                    "<< /Length {filler} >>\nstream\n{}\nendstream",
                    " ".repeat(filler)
                ),
            ),
        ];
        file_with_stream_table(&objects, &[(5, 6, 0)])
    }

    /// A stream read again through an owned handle reads what its dictionary names
    /// (here its `/DecodeParms`, in an object stream) through a reader of its own,
    /// which keeps it: the table it was first read through keeps nothing, nor does
    /// the handle, nor what readers apart from the table share.
    #[test]
    fn an_owned_stream_keeps_nothing_in_the_table_it_was_read_through() {
        let file = image_with_parameters_in_object_stream("/Length {len}", 0);
        let pdf = crate::Pdf::new(file).unwrap();
        let xref = pdf.xref();
        let stream = xref.get::<Stream<'_>>(ObjectIdentifier::new(4, 0)).unwrap();
        let before = kept_streams(xref);
        let apart = xref.detached();
        let owned = stream.to_owned_stream(Some(&apart));
        let reader = owned.reader();
        let read = reader.stream().unwrap();
        assert_eq!(read.decoded().unwrap().as_ref(), b"abc");
        assert_eq!(
            (
                kept_streams(xref),
                match &reader.read {
                    object::stream::Read::Indirect { xref, .. } => kept_streams(xref),
                    object::stream::Read::Inline { .. } => unreachable!(),
                },
                kept_streams(&apart.0)
            ),
            (before, 1, 0)
        );
        // Read through the table itself, the object stream is kept there.
        assert_eq!(stream.decoded().unwrap().as_ref(), b"abc");
        assert_eq!(kept_streams(xref), before + 1);
    }

    /// Where each `endstream` of the file is, looked for by readers apart from the
    /// table (here for an object stream with no /Length), is found once for the
    /// document, by the first of them, and kept in common with the table, which
    /// counts it apart: however many readers apart look for it, the file is searched
    /// once (each reader searched it again: work in the square of the pages of a
    /// document whose pages each need it).
    #[cfg(debug_assertions)]
    #[test]
    fn readers_apart_find_where_endstream_is_once_for_the_document() {
        use crate::work::BODIES_READ;
        let file = image_with_parameters_in_object_stream("", 64 << 10);
        let size = file.len();
        let pdf = crate::Pdf::new(file).unwrap();
        let xref = pdf.xref();
        let stream = xref.get::<Stream<'_>>(ObjectIdentifier::new(4, 0)).unwrap();
        assert_eq!(xref.apart_bytes(), 0);
        BODIES_READ.set(0);
        for _ in 0..8 {
            let reader = stream.to_owned_stream(Some(&xref.detached())).reader();
            assert_eq!(reader.stream().unwrap().decoded().unwrap().as_ref(), b"abc");
        }
        let read = BODIES_READ.get();
        assert!(read < 2 * size, "{read} bytes read of a file of {size}");
        assert!(xref.apart_bytes() > 0);
        // Read through the table, it is looked up there too.
        BODIES_READ.set(0);
        assert_eq!(stream.decoded().unwrap().as_ref(), b"abc");
        assert!(BODIES_READ.get() < size);
    }

    /// The size and content of the one page `file` opens to.
    fn one_page(file: Vec<u8>) -> (f64, f64, Vec<u8>) {
        let pdf = crate::Pdf::new(file).unwrap();
        let pages = pdf.pages();
        assert_eq!(pages.len(), 1);
        let media_box = pages[0].media_box();
        let content = pages[0].page_stream().unwrap_or_default().to_vec();
        (
            media_box.x1 - media_box.x0,
            media_box.y1 - media_box.y0,
            content,
        )
    }

    const CATALOG: &str = "<< /Type /Catalog /Pages 2 0 R >>";
    const TREE: &str = "<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
    const BLUE: &str = "0 0 1 rg 10 10 100 100 re f";
    const RED: &str = "1 0 0 rg 10 10 50 50 re f";

    // An entry into an object stream is the object of its number, whatever index the
    // entry names: the stream's header lists the number of each object it holds.
    #[test]
    fn an_object_stream_entry_reads_the_object_of_its_number() {
        let page = |size: &str, content: u32| {
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {size}] /Contents {content} 0 R >>"
            )
        };
        let (blue, red) = (page("600 700", 6), page("100 200", 7));
        let objects = |order: &[(u32, &str)]| {
            vec![
                (1, CATALOG.to_string()),
                (2, TREE.to_string()),
                (5, object_stream("/Length LEN", order)),
                (6, stream("", BLUE)),
                (7, stream("", RED)),
            ]
        };
        for (order, index) in [
            ([(3, blue.as_str()), (4, red.as_str())], 1),
            ([(3, blue.as_str()), (4, red.as_str())], 7),
            ([(4, red.as_str()), (3, blue.as_str())], 0),
        ] {
            let file =
                file_with_stream_table(&objects(&order), &[(3, 5, index), (4, 5, 0), (9, 5, 0)]);
            let pdf = crate::Pdf::new(file.clone()).unwrap();
            assert!(
                pdf.xref()
                    .get::<Object<'_>>(ObjectIdentifier::new(9, 0))
                    .is_none()
            );
            assert_eq!(one_page(file), (600.0, 700.0, BLUE.as_bytes().to_vec()));
        }
    }

    // An object stream whose /Length is an object it holds is read: reading the
    // length meets the stream being read, which fails that read only, and the stream
    // is read as one whose length is unknown.
    #[test]
    fn an_object_stream_holding_its_length_is_read() {
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 7 0 R >>";
        let holding = object_stream("/Length 6 0 R", &[(3, page), (6, "0")]);
        let length = holding.find("stream\n").unwrap() + 7;
        let length = holding.len() - length - "\nendstream".len();
        let holding = holding.replacen("0\n\nendstream", &format!("{length}\n\nendstream"), 1);
        let objects = vec![
            (1, CATALOG.to_string()),
            (2, TREE.to_string()),
            (5, holding),
            (7, stream("", BLUE)),
        ];
        let file = file_with_stream_table(&objects, &[(3, 5, 0), (6, 5, 1)]);
        assert_eq!(one_page(file), (600.0, 700.0, BLUE.as_bytes().to_vec()));
    }

    // A page after an object whose string never ends is found, though its own string
    // holds a header: rebuilding the table reads each object to its end.
    #[test]
    fn a_page_after_an_unterminated_string_is_found() {
        let body = format!(
            "1 0 obj\n{CATALOG}\nendobj\n2 0 obj\n{TREE}\nendobj\n4 0 obj\n(\nendobj\n\
             3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Title (8 0 obj) \
             /Contents 5 0 R >>\nendobj\n5 0 obj\n{}\nendobj\n",
            stream("", BLUE)
        );
        let file = format!("%PDF-1.7\n{body}trailer\n<< /Root 1 0 R >>\nstartxref\n9\n%%EOF\n");
        assert_eq!(
            one_page(file.into_bytes()),
            (600.0, 700.0, BLUE.as_bytes().to_vec())
        );
    }

    // Objects of an object stream each after a comment, as pdfTeX writes them (`% 3 0
    // obj`), are read: the comment starts the next object's slot, and is read as that
    // object's own reading reads it, so a dictionary before it is no stream's.
    #[test]
    fn objects_of_an_object_stream_after_comments_are_read() {
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 6 0 R >>";
        let commented = |number: u32, object: &str| format!("% {number} 0 obj\n{object}");
        let (catalog, tree, page) = (
            commented(1, CATALOG),
            commented(2, TREE),
            commented(3, page),
        );
        for order in [
            [
                (1, catalog.as_str()),
                (2, tree.as_str()),
                (3, page.as_str()),
            ],
            [
                (3, page.as_str()),
                (2, tree.as_str()),
                (1, catalog.as_str()),
            ],
        ] {
            let objects = vec![
                (5, object_stream("/Length LEN", &order)),
                (6, stream("", BLUE)),
            ];
            let indices: Vec<(u32, u32, u32)> = order
                .iter()
                .enumerate()
                .map(|(index, (number, _))| (*number, 5, index as u32))
                .collect();
            let file = file_with_stream_table(&objects, &indices);
            assert_eq!(one_page(file), (600.0, 700.0, BLUE.as_bytes().to_vec()));
        }
    }

    // Reading every object of an object stream reads each byte of it a bounded number
    // of times where comments follow dictionaries: one in the next object's slot is
    // read as far as that slot, not on along its line, past the slots after it, which
    // each dictionary would read again (`<<>>%<<>>%…` in one line).
    #[test]
    fn objects_of_an_object_stream_after_comments_are_read_in_linear_time() {
        // The header and the data of an object stream of about `n` objects.
        type Objects = fn(usize) -> (String, String);
        let families: [(&str, Objects); 2] = [
            ("a comment before each object", |n| {
                let object = "% 9 0 obj\n<< /A 1 >>\n";
                let header = (0..n).map(|i| format!("{} {} ", 10 + i, i * object.len()));
                (header.collect(), object.repeat(n))
            }),
            ("dictionaries and comments in one line", |n| {
                let header = (0..2 * n).map(|i| format!("{} {} ", 10 + i, i / 2 * 5 + i % 2 * 4));
                let line = format!("{}{}\n", "<<>>%".repeat(n), " x".repeat(4 * n));
                (header.collect(), line)
            }),
        ];
        for (family, objects) in families {
            let [small, large] = [1000, 2000].map(|n| {
                let (header, data) = objects(n);
                let count = header.split(' ').filter(|n| !n.is_empty()).count() / 2;
                let stream = object_stream_headed(&header, &data, count);
                let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";
                let objects = vec![
                    (1, CATALOG.to_string()),
                    (2, TREE.to_string()),
                    (3, page.to_string()),
                    (4, stream),
                ];
                let entries: Vec<(u32, u32, u32)> =
                    (0..count as u32).map(|i| (10 + i, 4, i)).collect();
                let file = file_with_stream_table(&objects, &entries);
                let pdf = crate::Pdf::new(file).unwrap();
                crate::work::EXAMINED.set(0);
                crate::work::BODIES_READ.set(0);
                let mut read = 0;
                for i in 0..count {
                    read += usize::from(pdf.xref().get::<Dict<'_>>(id(10 + i as i32)).is_some());
                }
                let work = crate::work::EXAMINED.get() + crate::work::BODIES_READ.get();
                assert!(
                    work <= 16 * data.len(),
                    "{family}: {work} steps for {} bytes",
                    data.len()
                );
                (work, read)
            });
            assert!(
                large.0 <= small.0 * 25 / 10,
                "{family}: {small:?} steps and objects read for 1000, {large:?} for 2000"
            );
            if family == "a comment before each object" {
                assert_eq!((small.1, large.1), (1000, 2000));
            }
        }
    }

    // A rebuild takes no header in a stream's data for one of the file's, where the
    // data end at the `endstream` /Length leads to, a reference read as the rebuilt
    // table holds it, or else at the first `endstream` where no other stream starts
    // before it, as PyMuPDF 1.28.2 does; it finds one after the data so ended, and one
    // in data that nothing ends, as the walk reads such data.
    #[test]
    fn a_rebuild_takes_no_header_in_a_streams_data_for_the_files() {
        let header = "9 0 obj\n<< /Type /Font >>\nendobj\n";
        let later = "10 0 obj\n<< /Length 1 >>\nstream\nx\nendstream";
        let length = header.len();
        let cases = [
            (
                format!("<< /Length {length} >>\nstream\n{header}\nendstream"),
                false,
            ),
            (
                format!("<< /Length 5 0 R >>\nstream\n{header}\nendstream"),
                false,
            ),
            (
                format!("<< /Length 1 >>\nstream\n{header}\nendstream"),
                false,
            ),
            (
                format!("<< /Length 6 0 R >>\nstream\n{header}\nendstream"),
                false,
            ),
            (format!("<< >>\nstream\n{header}\nendstream"), false),
            // A word holding `stream` is no stream's start.
            (
                format!("<< /Length 1 >>\nstream\n(upstream){header}\nendstream"),
                false,
            ),
            (
                format!("<< /Length 1 >>\nstream\nx\nendstream\n{header}\nendstream"),
                true,
            ),
            // No `endstream` follows: the data are read, where /Length leads to the
            // `endobj` after them too.
            (format!("<< /Length {length} >>\nstream\n{header}"), true),
            (format!("<< /Length 1 >>\nstream\nx\n{header}"), true),
            // The data's `endstream` is misspelled or missing: the first after the data
            // is a later stream's, which starts before it, and the header the file's,
            // whatever /Length is.
            (
                format!("<< /Length 1 >>\nstream\nx\nendsteam\nendobj\n{header}{later}"),
                true,
            ),
            (
                format!("<< /Length 1 >>\nstream\nx\nendobj\n{header}{later}"),
                true,
            ),
            (
                format!("<< /Length 6 0 R >>\nstream\nx\nendobj\n{header}{later}"),
                true,
            ),
            (
                format!("<< /Length 5 0 R >>\nstream\nx\n{header}{later}"),
                true,
            ),
            // /Length runs past the data's `endstream` to the header's `endobj`, as in
            // pdf.js's issue2098.pdf: the data end at that `endstream`.
            (
                format!(
                    "<< /Length {} >>\nstream\nx\nendstream\nendobj\n{header}",
                    "x\nendstream\nendobj\n".len() + header.len() - "endobj\n".len()
                ),
                true,
            ),
        ];
        for (object, found) in cases {
            let body = format!("4 0 obj\n{object}\nendobj\n5 0 obj\n{length}\nendobj\n");
            let pdf = crate::Pdf::new(file_with(body.as_bytes(), &[0], true)).unwrap();
            assert_eq!(pdf.xref().has_entry(id(9)), found, "{object}");
            assert!(pdf.xref().has_entry(id(5)), "{object}");
        }
    }

    // A stream whose data no `endstream` ends, missing or misspelled, hides none of the
    // objects after it, though a later stream's `endstream` is the first after the
    // data, whatever its /Length: a reference resolved to the length of its data, to a
    // wrong one, or to no object, or a number, right or wrong. The page and its content
    // stream after it are found, as the walk before 9b38fdbe found them.
    #[test]
    fn a_stream_without_endstream_hides_no_later_object() {
        for (length, object) in [
            ("7 0 R", "7 0 obj\n1\nendobj\n"),
            ("7 0 R", "7 0 obj\n40\nendobj\n"),
            ("7 0 R", ""),
            ("1", ""),
            ("40", ""),
        ] {
            for end in ["\nendobj", "\nendsteam\nendobj", ""] {
                let body = format!(
                    "4 0 obj\n<< /Length {length} >>\nstream\nx{end}\n\
                     3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 8 0 R >>\nendobj\n\
                     8 0 obj\n<< /Length 25 >>\nstream\n1 0 0 rg 0 0 100 100 re f\nendstream\nendobj\n{object}"
                );
                let pdf = crate::Pdf::new(file_with(body.as_bytes(), &[0], true)).unwrap();
                let page = pdf.xref().get::<Dict<'_>>(id(3)).unwrap();
                assert!(page.contains_key(object::dict::keys::CONTENTS), "{body}");
                assert!(pdf.xref().has_entry(id(8)), "{body}");
            }
        }
    }

    // A PDF attached to the document uncompressed is the data of a stream, not part of
    // the document: a rebuild takes none of its objects, catalog or trailer, though
    // they are numbered as the document's and come after them, where the later of two
    // objects numbered alike would be the table's. The document keeps its own page, as
    // PyMuPDF 1.28.2 shows it where /Length is a number or the attached PDF's first
    // `endstream` is its last. A /Length that is a reference is read as the rebuilt
    // table holds it: the document's object, before the stream or after it, after a
    // comment or not, though an object numbered alike whose value leads to the
    // attached PDF's first `endstream` follows that `endstream`, in the attached PDF,
    // or is in the data of a later stream, which the walk skips.
    #[test]
    fn a_rebuild_takes_no_object_of_an_embedded_pdf() {
        let content = "0 0 1 rg 0 0 300 200 re f";
        for content_first in [false, true] {
            for inner_length in [content.len().to_string(), "5 0 R".to_string()] {
                for decoy_inside in [false, true] {
                    let objects = attached_objects(content, content_first, &inner_length);
                    let inner = attached_pdf(&objects, decoy_inside);
                    embedded_in_document(&inner);
                }
            }
        }
    }

    /// The objects of a PDF of one 300 × 200 page drawn by `content`, numbered from 1,
    /// with its content stream first or last, and its /Length `length`: a number, or
    /// `5 0 R`, object 5 then following the content stream.
    fn attached_objects(content: &str, content_first: bool, length: &str) -> Vec<(u32, String)> {
        let mut objects = vec![
            (1, CATALOG.to_string()),
            (2, TREE.to_string()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R >>"
                    .to_string(),
            ),
        ];
        let mut tail = vec![(
            4,
            format!("<< /Length {length} >>\nstream\n{content}\nendstream"),
        )];
        if length == "5 0 R" {
            tail.push((5, content.len().to_string()));
        }
        if content_first {
            tail.append(&mut objects);
            tail
        } else {
            objects.append(&mut tail);
            objects
        }
    }

    /// A PDF of `objects`, with its table and trailer; with `decoy`, an object 5 after
    /// its first `endstream`, whose value leads there from the PDF's start.
    fn attached_pdf(objects: &[(u32, String)], decoy: bool) -> Vec<u8> {
        let mut inner = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![];
        for (number, object) in objects {
            offsets.push((*number, inner.len()));
            inner.extend(format!("{number} 0 obj\n{object}\nendobj\n").bytes());
            if decoy && object.contains("endstream") {
                let first = inner.windows(9).position(|w| w == b"endstream").unwrap();
                offsets.push((5, inner.len()));
                inner.extend(format!("5 0 obj\n{first}\nendobj\n").bytes());
            }
        }
        offsets.sort();
        let xref = inner.len();
        inner.extend(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).bytes());
        for (_, offset) in offsets {
            inner.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        inner.extend(
            format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes(),
        );
        inner
    }

    /// Checks that a document whose table is rebuilt, holding `inner` uncompressed as
    /// its object 4, opens to its own page, its object 4 the attached file and its
    /// length object 5 its own: whether /Length is a number or a reference to 5, whose
    /// value follows a comment or not, before the stream or after it, before or after
    /// a later stream, whose right /Length is a number, holding an object 5 whose value
    /// leads to the first `endstream` of `inner`, or no such stream.
    fn embedded_in_document(inner: &[u8]) {
        let first = inner.windows(9).position(|w| w == b"endstream").unwrap();
        let held = format!("5 0 obj {first} endobj");
        let decoy = format!(
            "8 0 obj\n<< /Length {} >>\nstream\n{held}\nendstream\nendobj\n",
            held.len()
        );
        for place in ["direct", "before", "after", "after the later stream"] {
            for comment in ["", "% the length\n"] {
                for later in ["", decoy.as_str()] {
                    if place == "direct" && !comment.is_empty() {
                        continue;
                    }
                    let length_object = format!("5 0 obj {comment}{}\nendobj\n", inner.len());
                    let length = if place == "direct" {
                        inner.len().to_string()
                    } else {
                        "5 0 R".to_string()
                    };
                    let mut body = vec![];
                    if place == "before" {
                        body.extend(length_object.bytes());
                    }
                    body.extend(
                        format!(
                            "4 0 obj\n<< /Type /EmbeddedFile /Subtype /application#2Fpdf /Length {length} >>\nstream\n"
                        )
                        .bytes(),
                    );
                    body.extend(inner);
                    body.extend(b"\nendstream\nendobj\n");
                    if place == "after" {
                        body.extend(length_object.bytes());
                    }
                    body.extend(later.bytes());
                    if place == "after the later stream" {
                        body.extend(length_object.bytes());
                    }
                    let file = file_with(&body, &[0], true);
                    let case = format!("{place}, {comment:?}, {later:?}");
                    // Opens to the document's 100 × 100 page.
                    work(&file);
                    let pdf = crate::Pdf::new(file).unwrap();
                    let attached = pdf.xref().get::<Dict<'_>>(id(4)).unwrap();
                    assert!(attached.contains_key(object::dict::keys::SUBTYPE), "{case}");
                    if place != "direct" {
                        assert_eq!(
                            pdf.xref().get::<u32>(id(5)),
                            Some(inner.len() as u32),
                            "{case}"
                        );
                    }
                }
            }
        }
    }

    // The header a stream's /Length was read from stays in the table: a later stream
    // whose data hold it, and whose own /Length would end them after it, has its data
    // read, as taking them out would take the first stream's length object out too.
    #[test]
    fn a_length_object_in_a_later_streams_data_stays_in_the_table() {
        let objects = attached_objects("0 0 1 rg 0 0 300 200 re f", true, "25");
        let inner = attached_pdf(&objects, false);
        let held = format!("x stream\n7 0 obj {} endobj\n", inner.len());
        let mut body = b"4 0 obj\n<< /Type /EmbeddedFile /Length 7 0 R >>\nstream\n".to_vec();
        body.extend(&inner);
        body.extend(
            format!(
                "\nendstream\nendobj\n8 0 obj\n<< /Length 9 0 R >>\nstream\n{held}\nendstream\nendobj\n\
                 9 0 obj {} endobj\n",
                held.len() + 1
            )
            .bytes(),
        );
        let file = file_with(&body, &[0], true);
        work(&file);
        let pdf = crate::Pdf::new(file).unwrap();
        assert_eq!(pdf.xref().get::<u32>(id(7)), Some(inner.len() as u32));
    }

    // A rebuild does not lex a stream's data: opening a file that is mostly the data
    // of a stream full of numbers and headers lexes a bounded part of it, however
    // long the data ([`crate::work::EXAMINED`]).
    #[test]
    fn a_rebuild_does_not_lex_a_streams_data() {
        for n in [1024, 4096] {
            let data = "12 0 obj [1 0 R] ".repeat(n);
            let stream = format!(
                "4 0 obj\n<< /Length {} >>\nstream\n{data}\nendstream\nendobj\n",
                data.len()
            );
            work(&file_with(stream.as_bytes(), &[0], true));
            let lexed = crate::work::EXAMINED.get();
            assert!(
                lexed < 4096,
                "{lexed} bytes lexed to open {} bytes",
                stream.len()
            );
        }
    }

    // Entries whose objects each hold, in a string, the header the next entry leads
    // to: a body is read in its slot, up to that header, where its string does not
    // end, so only the innermost object is read, and each other is lost, which its
    // read tells. Reading each to its end would read the shared tail once for each
    // entry, in time and memory quadratic in the file ([`Place`]).
    #[test]
    fn entries_into_a_string_of_another_object_read_only_the_innermost() {
        for space in [false, true] {
            let pdf = crate::Pdf::new(nested(30, space, false)).unwrap();
            for number in 4..34 {
                let id = ObjectIdentifier::new(number, 0);
                let (string, lost) = pdf
                    .xref()
                    .watch(|| pdf.xref().get::<object::String<'_>>(id));
                let expected = if number == 33 {
                    (true, None)
                } else {
                    (false, Some(id))
                };
                assert_eq!(
                    (string.is_some(), lost),
                    expected,
                    "object {number}, space {space}"
                );
            }
        }
    }

    // Opening a file whose table is kept lexes no body it does not read: the check
    // reads headers only, so an object nothing reads costs nothing to open, however
    // large it is.
    #[test]
    fn opening_a_file_lexes_no_object_it_does_not_read() {
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";
        let opened = |n: usize| {
            let unread = format!("<< {}>>", "/K [1 (s) <41>] ".repeat(n));
            let file = file_of(&[CATALOG, TREE, page, &unread], false);
            crate::work::EXAMINED.set(0);
            let pdf = crate::Pdf::new(file).unwrap();
            assert_eq!(pdf.pages().len(), 1);
            crate::work::EXAMINED.get()
        };
        // Sizes whose `startxref` offsets have as many digits, which opening lexes.
        assert_eq!(opened(1024), opened(4096));
    }

    /// A file of `objects`, numbered from 1, the first being the catalog, with a table
    /// listing them, or, where `broken`, a `startxref` that leads to none, which
    /// rebuilds it.
    fn file_of(objects: &[&str], broken: bool) -> Vec<u8> {
        let headed: Vec<(String, &str)> = objects
            .iter()
            .enumerate()
            .map(|(i, object)| (format!("{} 0 obj", i + 1), *object))
            .collect();
        file_headed(&headed, broken)
    }

    /// As [`file_of`], each object after its header.
    fn file_headed(objects: &[(String, &str)], broken: bool) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![];
        for (header, object) in objects {
            offsets.push(out.len());
            out.extend(format!("{header}\n{object}\nendobj\n").bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            out.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        let start = if broken { 9 } else { xref };
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        out
    }

    // A page holding a name whose `#` is no escape is read as one holding a name
    // without it, whether its table is sound or rebuilt: the name is a name, as
    // `MuPDF` reads it (`PyMuPDF` 1.28.2), here a key without a value.
    #[test]
    fn a_page_holding_a_name_that_is_no_key_is_read() {
        let content = stream("", BLUE);
        for page in [
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R /Broken#zz >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R \
             /Resources << /Broken#zz >> >>",
            "<< /Type /Page /Parent 2 0 R /Broken#zz /MediaBox [0 0 600 700] /Contents 4 0 R >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R /Broken#zz 5 >>",
        ] {
            for broken in [false, true] {
                let unescaped = page.replace("#zz", "zz");
                let read = |page: &str| one_page(file_of(&[CATALOG, TREE, page, &content], broken));
                assert_eq!(read(page), read(&unescaped), "{page}, rebuilt: {broken}");
            }
        }
    }

    // A header whose numbers end in a minus, which reading a number reads, leads to
    // its object whether the table is sound or rebuilt: checking the table, rebuilding
    // it and reading an object read a header by one rule.
    #[test]
    fn a_page_whose_header_numbers_end_in_a_minus_is_read() {
        let content = stream("", BLUE);
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R >>";
        for header in ["3- 0 obj", "3 0- obj", "3-- 0-- obj"] {
            for broken in [false, true] {
                let objects = [
                    ("1 0 obj".to_owned(), CATALOG),
                    ("2 0 obj".to_owned(), TREE),
                    (header.to_owned(), page),
                    ("4 0 obj".to_owned(), content.as_str()),
                ];
                let file = file_headed(&objects, broken);
                assert_eq!(
                    one_page(file.clone()),
                    (600.0, 700.0, BLUE.as_bytes().to_vec()),
                    "{header}, rebuilt: {broken}"
                );
                let pdf = crate::Pdf::new(file).unwrap();
                let page = &pdf.pages()[0];
                assert_eq!(page.obj_id(), Some(ObjectIdentifier::new(3, 0)));
                assert_eq!(page.try_page_stream().unwrap(), BLUE.as_bytes());
            }
        }
    }

    // Wherever reading a header finds object 3, so does a rebuild that scans the file
    // for headers: it looks for one wherever a number can start, signs included,
    // and reads it as reading does.
    #[test]
    fn a_page_whose_header_numbers_are_signed_is_read() {
        let content = stream("", BLUE);
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R >>";
        let mut headers = 0;
        for number in ["3", "+3", "3-", "+3-", "03", "+03", "3.0", "+3.0", "-3"] {
            for generation in ["0", "+0", "-0", "0-", "+0.0"] {
                let header = format!("{number} {generation} obj");
                let read =
                    Reader::new(header.as_bytes()).read_without_context::<ObjectIdentifier>();
                if read != Some(ObjectIdentifier::new(3, 0)) {
                    continue;
                }
                headers += 1;
                for broken in [false, true] {
                    let objects = [
                        ("1 0 obj".to_owned(), CATALOG),
                        ("2 0 obj".to_owned(), TREE),
                        (header.clone(), page),
                        ("4 0 obj".to_owned(), content.as_str()),
                    ];
                    assert_eq!(
                        one_page(file_headed(&objects, broken)),
                        (600.0, 700.0, BLUE.as_bytes().to_vec()),
                        "{header}, rebuilt: {broken}"
                    );
                }
            }
        }
        assert_eq!(headers, 40);
    }

    /// What reading the content of the one page `file` opens to finds.
    fn content_of(file: Vec<u8>) -> Result<Vec<u8>, crate::page::ContentError> {
        let pdf = crate::Pdf::new(file).unwrap();
        let pages = pdf.pages();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].media_box().x1, 600.0);
        pages[0].try_page_stream()
    }

    // An object whose header is in the file but which cannot be read stays one that
    // cannot be read when the table is rebuilt, whether the old table lists it or
    // only the scan finds it: a page whose content it is, alone or as one part, is no
    // empty page. A reference to an object the file does not hold is still `null`.
    #[test]
    fn content_that_cannot_be_read_is_not_taken_for_missing_content() {
        use crate::page::ContentError;
        let blue = stream("", BLUE);
        let page = |contents: &str| {
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents {contents} >>")
        };
        for broken_object in [
            format!("[ 1 2\n{blue}"),
            format!("<< /Length >\nstream\n{BLUE}\nendstream"),
        ] {
            for contents in ["4 0 R", "[5 0 R 4 0 R]"] {
                for broken in [false, true] {
                    let page = page(contents);
                    let file = file_of(&[CATALOG, TREE, &page, &broken_object, &blue], broken);
                    let read = content_of(file);
                    assert!(
                        matches!(read, Err(ContentError::Unreadable(Some(id))) if id == ObjectIdentifier::new(4, 0)),
                        "{broken_object:?} in {contents}, rebuilt: {broken}: {read:?}"
                    );
                }
            }
        }
        for (contents, content) in [
            ("9 0 R", vec![]),
            ("[5 0 R 9 0 R]", format!("{BLUE} ").into_bytes()),
        ] {
            for broken in [false, true] {
                let page = page(contents);
                let file = file_of(&[CATALOG, TREE, &page, "null", &blue], broken);
                assert_eq!(
                    content_of(file).unwrap(),
                    content,
                    "{contents}, rebuilt: {broken}"
                );
            }
        }
    }

    // What a page is drawn with that the file lists but that cannot be read, an entry
    // it inherits or a category of its resources, stays lost: the page reads as
    // viewers draw it, but tells what it lost, whether the table is sound or rebuilt.
    // A page tree entry that cannot be read may have been a node that held pages:
    // the document tells it. A reference to an object the file does not hold is still
    // `null`, and an entry the page is not drawn with, such as /Thumb, loses nothing.
    #[test]
    fn what_a_page_is_drawn_with_that_cannot_be_read_is_not_taken_for_missing() {
        let blue = stream("", BLUE);
        let font = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";
        let page = |entries: &str| {
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R {entries} >>"
            )
        };
        let tree = |entries: &str| format!("<< /Type /Pages /Kids [3 0 R] /Count 1 {entries} >>");
        let lost = Some(ObjectIdentifier::new(5, 0));
        let cases = [
            // The page's resources.
            (
                tree(""),
                page("/Resources 5 0 R"),
                "<< /Font << /F1 6 0 R >> /Broken >",
                lost,
            ),
            // Their fonts.
            (
                tree(""),
                page("/Resources << /Font 5 0 R >>"),
                "<< /F1 6 0 R /Broken >",
                lost,
            ),
            // Resources the page inherits.
            (
                tree("/Resources 5 0 R"),
                page(""),
                "<< /Font << /F1 6 0 R >> /Broken >",
                lost,
            ),
            // A box it inherits.
            (tree("/CropBox 5 0 R"), page(""), "[0 0 600 700", lost),
            // The page itself, which the tree loses.
            (
                tree(""),
                "<< /Type /Page /MediaBox [0 0 600 700] /Contents 4 0 R /Broken >".to_owned(),
                "null",
                None,
            ),
            // Resources the file does not hold, and a thumbnail that cannot be read.
            (tree(""), page("/Resources 9 0 R"), "null", None),
            (tree(""), page("/Thumb 5 0 R"), "<< /Broken >", None),
            (tree("/Resources 9 0 R"), page(""), "null", None),
        ];
        for (tree, page, fifth, unreadable) in &cases {
            for broken in [false, true] {
                let file = file_of(&[CATALOG, tree, page, &blue, fifth, font], broken);
                let pdf = crate::Pdf::new(file).unwrap();
                let pages = pdf.pages();
                assert_eq!(pages.len(), 1);
                assert_eq!(
                    pages[0].unreadable(),
                    *unreadable,
                    "{tree} {page} {fifth}, rebuilt: {broken}"
                );
                let tree_lost = page
                    .ends_with("/Broken >")
                    .then(|| ObjectIdentifier::new(3, 0));
                assert_eq!(
                    pages.unreadable(),
                    tree_lost,
                    "{tree} {page} {fifth}, rebuilt: {broken}"
                );
            }
        }
        // A page the file does not hold is an empty page that lost nothing.
        let file = file_of(
            &[CATALOG, "<< /Type /Pages /Kids [9 0 R] /Count 1 >>"],
            false,
        );
        let pdf = crate::Pdf::new(file).unwrap();
        assert_eq!(pdf.pages().len(), 1);
        assert_eq!(pdf.pages()[0].unreadable(), None);
        assert_eq!(pdf.pages().unreadable(), None);
        assert!(pdf.pages()[0].raw().is_empty());
    }

    #[test]
    fn find_last_xref_uses_last_startxref() {
        let pdf = b"%PDF-1.0\nstartxref\n5\n%%EOF\nstartxref\n42\n%%EOF";
        assert_eq!(find_last_xref_pos(pdf), Some(42));
    }

    #[test]
    fn xref_table_trailer_rejects_overflowing_entry_skip() {
        let data = b"xref\n0 999999999999999999999\ntrailer\n<<>>";
        let mut reader = Reader::new(data);
        assert!(read_xref_table_trailer(&mut reader, &ReaderContext::dummy()).is_none());
    }

    /// A file whose root lists node 3 and `n` entries the table puts at it, a node of
    /// one 600 × 700 page whose header is padded to `64 * n` bytes where `padded`.
    fn aliased_padded(n: u32, padded: bool, encrypted: bool) -> Vec<u8> {
        use super::rc4::{self, Part};
        let kids: Vec<String> = (0..=n)
            .map(|i| format!("{} 0 R", if i == 0 { 3 } else { 4 + i }))
            .collect();
        let root = format!("<< /Type /Pages /Kids [{}] >>", kids.join(" "));
        let aliases: Vec<(u32, u32)> = (1..=n).map(|i| (4 + i, 3)).collect();
        let pad = if padded { 64 * n as usize } else { 1 };
        rc4::file_headed(
            &[
                Part::Object("<< /Type /Catalog /Pages 2 0 R >>", b""),
                Part::Object(&root, b""),
                Part::Object("<< /Type /Pages /Kids [4 0 R] /Rotate 0 >>", b""),
                Part::Object(
                    "<< /Type /Page /Parent 3 0 R /MediaBox [0 0 600 700] >>",
                    b"",
                ),
            ],
            &aliases,
            encrypted,
            &|number| {
                let pad = if number == 3 { pad } else { 1 };
                format!("{number}{}0 obj", " ".repeat(pad))
            },
        )
    }

    /// The bytes lexed to open `file` and find its pages' sizes, once it is checked
    /// that each of the `pages` pages is 600 × 700 pt.
    fn lexed_to_open(file: Vec<u8>, pages: usize) -> usize {
        crate::work::EXAMINED.set(0);
        let pdf = crate::Pdf::new(file).unwrap();
        assert_eq!(pdf.pages().len(), pages);
        for page in pdf.pages().iter() {
            assert_eq!(page.render_dimensions(), (600.0, 700.0));
        }
        crate::work::EXAMINED.get()
    }

    // The header of an object that entries of an encrypted file's table lead to,
    // which names the key its bytes decrypt with, is read once for them all, not once
    // for each: reading the file is linear in its size, however long the header.
    #[test]
    fn a_header_entries_lead_to_is_read_once_for_them_all() {
        for (padded, encrypted) in [(true, true), (true, false), (false, true)] {
            let [small, large] = [256, 512]
                .map(|n| lexed_to_open(aliased_padded(n, padded, encrypted), n as usize + 1));
            assert!(
                large <= small * 25 / 10,
                "padded: {padded}, encrypted: {encrypted}: bytes lexed for 256 \
                 entries {small}, for 512 {large}"
            );
        }
    }

    // An object of another kind than a read asks for is read once to find out it is
    // not lost, however many references lead to it.
    #[test]
    fn an_object_of_another_kind_is_read_once_to_find_it_is_not_lost() {
        let n = 512;
        let items = vec!["5 0 R"; n].join(" ");
        let annots = format!("<< /List [{items}] >>");
        let string = format!("({})", "\\101".repeat(64 * n));
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";
        let file = file_of(&[CATALOG, TREE, page, &annots, &string], false);
        let pdf = crate::Pdf::new(file).unwrap();
        let list = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(4, 0))
            .unwrap()
            .get::<Array<'_>>(b"List".as_slice())
            .unwrap();
        body::BODIES.with_borrow_mut(hash::Map::clear);
        let read: Vec<_> = list.iter_checked::<Dict<'_>>().collect();
        assert!(read.is_empty(), "{read:?}");
        let bodies =
            body::BODIES.with_borrow(|bodies| bodies.values().copied().collect::<Vec<_>>());
        assert_eq!(bodies, [1]);
    }

    /// An object stream whose header is `header` and whose objects follow it as
    /// `objects`, with `count` objects.
    fn object_stream_headed(header: &str, objects: &str, count: usize) -> String {
        format!(
            "<< /Type /ObjStm /N {count} /First {} /Length {} >>\nstream\n{header}{objects}\nendstream",
            header.len(),
            header.len() + objects.len()
        )
    }

    fn id(number: i32) -> ObjectIdentifier {
        ObjectIdentifier::new(number, 0)
    }

    // An entry into an object stream whose header puts the object past the stream's
    // data leads to no object that can be read: the object is lost, however it is
    // read, not missing, so a page sized or listed by it is not taken for one
    // without it.
    #[test]
    fn an_object_stream_entry_past_its_data_is_lost() {
        for (at, lost) in [(9999, true), (0, false)] {
            let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /CropBox 5 0 R >>";
            let crop = object_stream_headed(&format!("5 {at} "), "[0 0 400 500]", 1);
            let objects = vec![
                (1, CATALOG.to_string()),
                (2, TREE.to_string()),
                (3, page.to_string()),
                (4, crop),
            ];
            let pdf = crate::Pdf::new(file_with_stream_table(&objects, &[(5, 4, 0)])).unwrap();
            let page = &pdf.pages()[0];
            if lost {
                assert_eq!(page.unreadable(), Some(id(5)));
            } else {
                assert_eq!(page.unreadable(), None);
                assert_eq!(page.crop_box().x1, 400.0);
            }
            let xref = pdf.xref();
            for _ in 0..2 {
                let (_, read) = xref.watch(|| xref.get::<Array<'_>>(id(5)));
                assert_eq!(read, lost.then_some(id(5)), "at {at}");
            }

            let tree = "<< /Type /Pages /Kids [3 0 R 6 0 R] >>";
            let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] >>";
            let objects = vec![
                (1, CATALOG.to_string()),
                (2, tree.to_string()),
                (3, page.to_string()),
                (4, object_stream_headed(&format!("5 {at} "), "[7 0 R]", 1)),
                (
                    6,
                    "<< /Type /Pages /Parent 2 0 R /Kids 5 0 R >>".to_string(),
                ),
                (7, "<< /Type /Page /Parent 6 0 R >>".to_string()),
            ];
            let pdf = crate::Pdf::new(file_with_stream_table(&objects, &[(5, 4, 0)])).unwrap();
            let pages = pdf.pages();
            if lost {
                assert_eq!((pages.len(), pages.unreadable()), (1, Some(id(5))));
            } else {
                assert_eq!((pages.len(), pages.unreadable()), (2, None));
            }
        }
    }

    // Entries the table puts at one index of an object stream, whose header lists
    // another object there, read each the object of its own number: what one of them
    // holds tells nothing of another, read before or after it.
    #[test]
    fn entries_at_one_index_of_an_object_stream_read_each_its_own_object() {
        for broken in [true, false] {
            let sixth = if broken { "<< /Broken >" } else { "<< /A 1 >>" };
            let body = format!("(readable)\n{sixth}\n");
            let header = format!("5 0 6 {} ", "(readable)\n".len());
            for order in [[5, 6], [6, 5]] {
                let objects = vec![
                    (1, CATALOG.to_string()),
                    (2, "<< /Type /Pages /Kids [] /Count 0 >>".to_string()),
                    (
                        3,
                        format!("<< /List [{} 0 R {} 0 R] >>", order[0], order[1]),
                    ),
                    (4, object_stream_headed(&header, &body, 2)),
                ];
                let file = file_with_stream_table(&objects, &[(5, 4, 0), (6, 4, 0)]);
                let pdf = crate::Pdf::new(file.clone()).unwrap();
                let xref = pdf.xref();
                for number in order {
                    let (_, lost) = xref.watch(|| xref.get::<Dict<'_>>(id(number)));
                    let expected = (broken && number == 6).then_some(id(6));
                    assert_eq!(lost, expected, "broken: {broken}, order {order:?}");
                }
                let pdf = crate::Pdf::new(file).unwrap();
                let xref = pdf.xref();
                let list = xref.get::<Dict<'_>>(id(3)).unwrap();
                let list = list.get::<Array<'_>>(b"List".as_slice()).unwrap();
                let read: Vec<_> = list.iter_checked::<Dict<'_>>().collect();
                let lost: Vec<_> = read.into_iter().filter_map(Result::err).collect();
                assert_eq!(lost, if broken { vec![id(6)] } else { vec![] });
            }
        }
    }

    /// The bytes lexed while every item of a page's /Annots, `n`
    /// references to string 4, whose header is padded with `64 * n` spaces where
    /// `padded`, is read as an annotation, as many times as it is listed.
    fn lexed_reading_annotations(n: usize, padded: bool) -> usize {
        let annots = vec!["4 0 R"; n].join(" ");
        let page =
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Annots [{annots}] >>");
        let pad = if padded {
            " ".repeat(512 * n)
        } else {
            " ".to_string()
        };
        let objects = [
            ("1 0 obj".to_string(), CATALOG),
            ("2 0 obj".to_string(), TREE),
            ("3 0 obj".to_string(), page.as_str()),
            (format!("4{pad}0 obj"), "(a)"),
        ];
        let pdf = crate::Pdf::new(file_headed(&objects, false)).unwrap();
        let page = &pdf.pages()[0];
        crate::work::EXAMINED.set(0);
        let annots = page.raw().get::<Array<'_>>(b"Annots".as_slice()).unwrap();
        for _ in 0..2 {
            assert_eq!(annots.iter_checked::<Dict<'_>>().count(), 0);
            assert_eq!(annots.iter::<Dict<'_>>().count(), 0);
        }
        crate::work::EXAMINED.get()
    }

    // The header of an object that many references lead to, read as another kind
    // than it is, is read once, however long: reading the references takes time
    // linear in the file.
    #[test]
    fn a_header_read_for_another_kind_of_object_is_read_once() {
        for padded in [true, false] {
            let [small, large] = [128, 256].map(|n| lexed_reading_annotations(n, padded));
            assert!(
                large <= small * 25 / 10,
                "padded: {padded}: bytes lexed {small} for 128 references, {large} for 256"
            );
        }
        // The padded header once, and the rest of the object.
        let bytes = lexed_reading_annotations(256, true);
        assert!(bytes <= 512 * 256 + 64, "{bytes} bytes lexed");
    }

    /// How often bodies were read, or streams' data found, while `run` ran, the most
    /// for one place.
    fn bodies_read(run: impl FnOnce()) -> usize {
        body::BODIES.with_borrow_mut(hash::Map::clear);
        body::DATA_FOUND.with_borrow_mut(hash::Map::clear);
        run();
        let most = |map: &hash::Map<Place, usize>| map.values().copied().max().unwrap_or(0);
        body::BODIES
            .with_borrow(most)
            .max(body::DATA_FOUND.with_borrow(most))
    }

    // A body read again and again, each read dropped before the next, is parsed
    // [`body::PARSES`] times, and kept from then on; one read while another holds
    // what it parsed is made of that, and parses nothing, however often it is read:
    // an array, a dictionary and a stream's dictionary alike, in the file or an
    // object stream. An object that holds no array nor dictionary is kept at once.
    #[test]
    fn a_body_is_parsed_three_times_at_most_and_not_while_a_read_holds_it() {
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";
        let stream = "<< /Length 3 /K [1 2] >>\nstream\nabc\nendstream";
        let objects = [
            CATALOG,
            TREE,
            page,
            "[1 /N (s)]",
            "<< /A [1] >>",
            stream,
            "42",
        ];
        let packed = [
            (1, CATALOG.to_string()),
            (2, TREE.to_string()),
            (3, page.to_string()),
            (
                20,
                object_stream_headed("4 0 5 11 ", "[1 /N (s)] << /A [1] >>", 2),
            ),
        ];
        let entries = [(4, 20, 0), (5, 20, 1)];
        for (file, numbers) in [
            (file_of(&objects, false), &[4, 5, 6, 7][..]),
            (file_with_stream_table(&packed, &entries), &[4, 5][..]),
        ] {
            let pdf = crate::Pdf::new(file).unwrap();
            for &number in numbers {
                let read = || pdf.xref().get::<Object<'_>>(id(number)).unwrap();
                let parsed = bodies_read(|| {
                    for _ in 0..8 {
                        drop(read());
                    }
                });
                let most = if number == 7 {
                    1
                } else {
                    usize::from(body::PARSES)
                };
                assert_eq!(parsed, most, "object {number}");
            }
        }
        let pdf = crate::Pdf::new(file_of(&objects, false)).unwrap();
        let read = || pdf.xref().get::<Object<'_>>(id(4)).unwrap();
        let parsed = bodies_read(|| {
            let held = read();
            for _ in 0..8 {
                assert_eq!(read(), held);
            }
        });
        assert_eq!(parsed, 1);
    }

    // Opening a file parses each body as often as the table parses one at most
    // ([`body::PARSES`]), the catalog too, which the rebuild of a table reads to
    // choose the trailer, reading the trailer reads, and the page tree reads, each
    // dropping it before the next parses it again: whether the table is read or
    // rebuilt, encrypted or not.
    #[test]
    fn opening_a_file_parses_each_body_a_bounded_number_of_times() {
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";
        let parts = [
            rc4::Part::Object(CATALOG, b""),
            rc4::Part::Object(TREE, b""),
            rc4::Part::Object(page, b""),
        ];
        // `file` with its `startxref` leading to no table, which rebuilds it.
        let broken = |mut file: Vec<u8>| {
            let tag = b"startxref\n";
            let at = file.windows(tag.len()).rposition(|w| w == tag).unwrap() + tag.len();
            let end = at + file[at..].iter().position(|b| *b == b'\n').unwrap();
            file.splice(at..end, *b"9");
            file
        };
        for encrypted in [false, true] {
            let file = rc4::file_with(&parts, &[], encrypted);
            for (rebuilt, file) in [(false, file.clone()), (true, broken(file))] {
                let most = bodies_read(|| {
                    let pdf = crate::Pdf::new(file).unwrap();
                    assert_eq!(pdf.pages().len(), 1);
                    assert_eq!(pdf.xref().is_encrypted(), encrypted);
                });
                assert!(
                    most <= usize::from(body::PARSES),
                    "encrypted {encrypted}, rebuilt {rebuilt}: a body read {most} times"
                );
            }
        }
    }

    // A number many entries of a damaged table lead to is read once for all of them,
    // however long it is written.
    #[test]
    fn a_number_entries_lead_to_is_read_once_for_them_all() {
        let n = 64;
        let number = format!("{}42", "0".repeat(512 * n));
        let parts = [
            rc4::Part::Object(CATALOG, b""),
            rc4::Part::Object(TREE, b""),
            rc4::Part::Object("<< /Type /Page /Parent 2 0 R >>", b""),
            rc4::Part::Object(&number, b""),
        ];
        let aliases: Vec<(u32, u32)> = (0..n as u32).map(|i| (5 + i, 4)).collect();
        for encrypted in [false, true] {
            let pdf = crate::Pdf::new(rc4::file_with(&parts, &aliases, encrypted)).unwrap();
            let xref = pdf.xref();
            let most = bodies_read(|| {
                for (alias, _) in &aliases {
                    for _ in 0..2 {
                        let read = xref.get::<i32>(id(*alias as i32));
                        assert_eq!(read, Some(42));
                    }
                }
            });
            assert_eq!(most, 1, "encrypted: {encrypted}");
        }
    }

    /// A generator of the same numbers on every run.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    /// What an object of a generated file is.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Kind {
        Dict,
        Array,
        Number,
        String,
        Name,
        Stream,
        /// A stream whose data cannot be read.
        StreamLost,
        /// An object that cannot be read.
        Broken,
    }

    impl Kind {
        fn body(self, rng: &mut Rng) -> String {
            let pad = " ".repeat(rng.below(3) * 40);
            match self {
                Self::Dict => format!("<< /K [1{pad}2] /S (x) >>"),
                Self::Array => format!("[1 (two){pad} /3]"),
                Self::Number => format!("{}42", "0".repeat(rng.below(3) * 40)),
                Self::String => format!("(s\\101{pad})"),
                Self::Name => format!("/N{}", "#41".repeat(rng.below(3) * 10)),
                Self::Stream => stream(&pad, "xyz"),
                Self::StreamLost => "<< /F (elsewhere) /Length 1 >>\nstream\nx\nendstream".into(),
                Self::Broken => format!("<< /Broken{pad} >"),
            }
        }
    }

    /// Reads entry `number` of `pdf` as every kind of object, twice each, and as an
    /// item of `list` and of a dictionary, and checks that what it finds is lost
    /// exactly where `kind` cannot be read as what is asked for, and is what reading
    /// it as any object finds; `None` for an entry that is lost however it is read.
    fn read_every_way(pdf: &crate::Pdf, number: i32, kind: Option<Kind>, at: &str) {
        let xref = pdf.xref();
        let id = id(number);
        let lost_as = |stream: bool| match kind {
            None | Some(Kind::Broken) => Some(id),
            Some(Kind::StreamLost) if stream => Some(id),
            _ => None,
        };
        macro_rules! read {
            ($t:ty, $stream:expr, $kinds:expr) => {
                for _ in 0..2 {
                    let (read, lost) = xref.watch(|| xref.get::<$t>(id));
                    assert_eq!(
                        lost,
                        lost_as($stream),
                        "{at}: {} of {number}",
                        stringify!($t)
                    );
                    let found = lost.is_none() && kind.is_some_and(|kind| $kinds.contains(&kind));
                    assert_eq!(
                        read.is_some(),
                        found,
                        "{at}: {} of {number}",
                        stringify!($t)
                    );
                }
            };
        }
        read!(
            Object<'_>,
            true,
            [
                Kind::Dict,
                Kind::Array,
                Kind::Number,
                Kind::String,
                Kind::Name,
                Kind::Stream
            ]
        );
        read!(Stream<'_>, true, [Kind::Stream]);
        read!(
            Dict<'_>,
            false,
            [Kind::Dict, Kind::Stream, Kind::StreamLost]
        );
        read!(Array<'_>, false, [Kind::Array]);
        read!(i32, false, [Kind::Number]);
        read!(object::Number, false, [Kind::Number]);
        read!(object::String<'_>, false, [Kind::String]);
        read!(Name<'_>, false, [Kind::Name]);
        read!(bool, false, [Kind::Broken; 0]);
        let reference = format!("[{number} 0 R]");
        let holder = Reader::new(reference.as_bytes())
            .read_with_context::<Array<'_>>(&ReaderContext::new(xref, false))
            .unwrap();
        let (_, lost) = xref.watch(|| holder.iter_checked::<Array<'_>>().count());
        assert_eq!(lost, lost_as(false), "{at}: item {number}");
        let (_, lost) = xref.watch(|| holder.iter::<Stream<'_>>().count());
        assert_eq!(lost, lost_as(true), "{at}: stream item {number}");
    }

    // Every read of an object the table lists, of any kind, through any of the
    // entries that lead to it, in the file or an object stream, whatever index or
    // offset the entry or the stream's header gives, parses its body as often as the
    // table parses a body at most ([`body::PARSES`]), and a read that finds a body
    // that cannot be read as what it asks for tells the computation being watched
    // that the object is lost.
    #[test]
    fn every_read_of_an_entry_parses_its_body_a_bounded_number_of_times_and_tells_what_it_lost() {
        const KINDS: [Kind; 8] = [
            Kind::Dict,
            Kind::Array,
            Kind::Number,
            Kind::String,
            Kind::Name,
            Kind::Stream,
            Kind::StreamLost,
            Kind::Broken,
        ];
        for seed in 1..=150_u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            // Objects of the file, from 3, and entries leading to them, which a
            // table that is rebuilt keeps, decrypting as the object they lead to.
            let count = 2 + rng.below(6);
            let kinds: Vec<Kind> = (0..count).map(|_| KINDS[rng.below(8)]).collect();
            let bodies: Vec<String> = kinds.iter().map(|kind| kind.body(&mut rng)).collect();
            let mut parts = vec![
                rc4::Part::Object(CATALOG, b""),
                rc4::Part::Object("<< /Type /Pages /Kids [] /Count 0 >>", b""),
            ];
            parts.extend(bodies.iter().map(|body| rc4::Part::Object(body, b"")));
            let aliases: Vec<(u32, u32)> = (0..rng.below(5))
                .map(|i| (3 + count as u32 + i as u32, 3 + rng.below(count) as u32))
                .collect();
            let pads: Vec<usize> = (0..parts.len()).map(|_| rng.below(3) * 50).collect();
            let encrypted = rng.below(2) == 0;
            let file = rc4::file_headed(&parts, &aliases, encrypted, &|number| {
                format!("{number}{}0 obj", " ".repeat(1 + pads[number as usize - 1]))
            });
            let pdf = crate::Pdf::new(file).unwrap();
            let kind_of = |number: u32| {
                let target = aliases
                    .iter()
                    .find(|(alias, _)| *alias == number)
                    .map_or(number, |(_, target)| *target);
                kinds[target as usize - 3]
            };
            let numbers: Vec<u32> = (3..3 + count as u32)
                .chain(aliases.iter().map(|(alias, _)| *alias))
                .collect();
            let most = bodies_read(|| {
                for _ in 0..2 {
                    for &number in &numbers {
                        let at = format!(
                            "seed {seed}, file, {kinds:?}, aliases {aliases:?}, encrypted {encrypted}"
                        );
                        read_every_way(&pdf, number as i32, Some(kind_of(number)), &at);
                    }
                }
            });
            assert!(
                most <= usize::from(body::PARSES),
                "seed {seed}: a body read {most} times"
            );
            // Entries leading to one object read the same object.
            for (alias, target) in &aliases {
                let xref = pdf.xref();
                let [alias, target] =
                    [*alias, *target].map(|n| xref.get::<Object<'_>>(id(n as i32)));
                assert_eq!(alias, target, "seed {seed}");
            }

            // Objects of an object stream, from 10, each listed in its header at its
            // offset, or past the stream's data, and entries putting each at its index
            // or another, and entries putting objects the stream does not hold in it.
            let kinds: Vec<Kind> = (0..2 + rng.below(6))
                .map(|_| {
                    [
                        Kind::Dict,
                        Kind::Array,
                        Kind::Number,
                        Kind::String,
                        Kind::Name,
                        Kind::Broken,
                    ][rng.below(6)]
                })
                .collect();
            let mut header = String::new();
            let mut data = String::new();
            let mut expected = vec![];
            for (i, kind) in kinds.iter().enumerate() {
                let past = rng.below(6) == 0;
                let offset = if past { 100_000 } else { data.len() };
                header += &format!("{} {offset} ", 10 + i);
                data += &kind.body(&mut rng);
                data.push('\n');
                expected.push((10 + i as i32, (!past).then_some(*kind)));
            }
            let mut compressed: Vec<(u32, u32, u32)> = (0..kinds.len())
                .map(|i| {
                    let index = if rng.below(3) == 0 {
                        rng.below(kinds.len() + 2)
                    } else {
                        i
                    };
                    (10 + i as u32, 4, index as u32)
                })
                .collect();
            for extra in 0..rng.below(3) {
                let number = 10 + kinds.len() as u32 + extra as u32;
                compressed.push((number, 4, rng.below(kinds.len() + 1) as u32));
                expected.push((number as i32, None));
            }
            let plain: Vec<Kind> = (0..rng.below(4))
                .map(|_| {
                    [
                        Kind::Dict,
                        Kind::Array,
                        Kind::Number,
                        Kind::String,
                        Kind::Name,
                        Kind::Stream,
                        Kind::StreamLost,
                    ][rng.below(7)]
                })
                .collect();
            let mut objects = vec![
                (1, CATALOG.to_string()),
                (2, "<< /Type /Pages /Kids [] /Count 0 >>".to_string()),
                (4, object_stream_headed(&header, &data, kinds.len())),
            ];
            for (i, kind) in plain.iter().enumerate() {
                objects.push((30 + i as u32, kind.body(&mut rng)));
                expected.push((30 + i as i32, Some(*kind)));
            }
            let pdf = crate::Pdf::new(file_with_stream_table(&objects, &compressed)).unwrap();
            let most = bodies_read(|| {
                for _ in 0..2 {
                    for (number, kind) in &expected {
                        let at = format!(
                            "seed {seed}, object stream {header}{data}, entries {compressed:?}"
                        );
                        read_every_way(&pdf, *number, *kind, &at);
                    }
                }
            });
            assert!(
                most <= usize::from(body::PARSES),
                "seed {seed}: a body read {most} times"
            );
        }
    }
}
