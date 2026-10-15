//! Reading and querying the xref table of a PDF file.

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
    AUTHOR, CREATION_DATE, CREATOR, ENCRYPT, FIRST, ID, INDEX, INFO, KEYWORDS, MOD_DATE, N,
    OCPROPERTIES, PAGES, PREV, PRODUCER, ROOT, SIZE, SUBJECT, TITLE, TYPE, VERSION, W, XREF_STM,
};
use crate::object::indirect::IndirectObject;
use crate::object::{Array, MaybeRef};
use crate::object::{DateTime, Dict};
use crate::object::{Object, ObjectLike};
use crate::pdf::PdfVersion;
use crate::reader::Reader;
use crate::reader::{Link, Readable, ReaderContext, ReaderExt};
use crate::sync::{Arc, Mutex, MutexExt, OnceLock};
use crate::trivia::{is_eol_character, is_white_space_character};
use crate::util::findr_needle;
use crate::{PdfData, object};
use alloc::borrow::Cow;
use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::max;
use core::iter;
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
        Ok(()) => match XRef::new(
            data.clone(),
            Rebuilt::sound(&xref_map),
            xref_map,
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

/// The entries of a table that is rebuilt.
#[derive(Debug, Default)]
struct Old {
    /// Those that lead to an object that ends before the next offset any entry names.
    listed: Vec<Listed>,
    /// Those that lead to a header, or the start of one that the next offset cuts,
    /// but to no object that ends before it: the scan reads such an object to its
    /// end where the header ends before the offset (`read_unread`).
    unread: Vec<Unfinished>,
}

/// An entry of a table that is rebuilt that leads to no object read before the next
/// offset any entry names, its slot.
#[derive(Debug, Clone, Copy)]
struct Unfinished {
    id: ObjectIdentifier,
    offset: usize,
    /// The header, where it ends in the slot, and where the object after it starts,
    /// where that is in the slot.
    header: Option<(ObjectIdentifier, Option<usize>)>,
}

/// An entry of a table that is rebuilt, which leads to an object.
#[derive(Debug, Clone, Copy)]
struct Listed {
    id: ObjectIdentifier,
    offset: usize,
    /// The identifier the object's header names, which may be another.
    header: ObjectIdentifier,
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

// Check that each entry leads to its object before any object is read: a table with
// an entry that does not is rebuilt, so the table never changes once read, and every
// value read from the document is read from the one table. Returns the entries, for
// the rebuilt table, if one does not lead to its own.
//
// An entry leads to its object where an object whose header names it is at its offset
// and reading it as the entry's object is read (`body::Shape::read`) reads it before the
// next offset any entry names: objects of a table do not overlap.
// So each header and object is read once however many entries name it, and no further
// than the next offset, its slot (`body::SlotReader`), which reads each byte of the file
// once. MuPDF, too, rebuilds a table with an entry past the end of the file or in an
// object stream the table does not list as an object (`check_xref_entry_offsets` in
// MuPDF's `pdf-xref.c`); an entry at offset 0 is free.
fn check(xref_map: &mut XrefMap, data: &[u8]) -> Result<(), Old> {
    xref_map.retain(|_, entry| *entry != EntryType::Normal(0));
    let mut entries = vec![];
    let mut streams = vec![];
    #[allow(clippy::iter_over_hash_type, reason = "sorted after")]
    for (id, entry) in xref_map.iter() {
        match *entry {
            EntryType::Normal(offset) => entries.push((offset, *id)),
            EntryType::ObjStream(stream, _) => streams.push(stream),
        }
    }
    entries.sort_unstable();

    let mut sound = true;
    let mut old = Old::default();
    // The entries at each offset, and the next offset.
    let mut rest = entries.as_slice();
    while let Some(&(offset, _)) = rest.first() {
        let at = rest.partition_point(|(other, _)| *other == offset);
        let end = rest.get(at).map_or(data.len(), |(next, _)| *next);
        let checked = object_in(data, offset, end);
        for &(_, id) in &rest[..at] {
            match checked {
                Checked::Object(header) => {
                    sound &= header == id;
                    old.listed.push(Listed { id, offset, header });
                }
                Checked::Unfinished(header) => {
                    sound = false;
                    old.unread.push(Unfinished { id, offset, header });
                }
                Checked::Nothing => sound = false,
            }
        }
        rest = &rest[at..];
    }
    if !streams.is_empty() {
        let objects: BTreeSet<i32> = entries.iter().map(|(_, id)| id.obj_number).collect();
        sound &= streams
            .iter()
            .all(|stream| i32::try_from(*stream).is_ok_and(|stream| objects.contains(&stream)));
    }

    if sound {
        Ok(())
    } else {
        warn!("xref table has an entry that does not lead to its object, rebuilding it");
        Err(old)
    }
}

/// What checking a table finds where an entry leads, in the entry's slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Checked {
    /// A header, the identifier it names, and an object after it that ends in the
    /// slot.
    Object(ObjectIdentifier),
    /// No object that ends in the slot, after a header that does, if one does, and
    /// where the object starts, if that is before the slot's end; else, after the
    /// start of a header that the slot's end cuts, which may be the entry's.
    Unfinished(Option<(ObjectIdentifier, Option<usize>)>),
    /// No header: the entry leads to no object.
    Nothing,
}

/// What the check of a table finds at `at` of `data`, whose slot ends at `slot`: the
/// header and the object after it, read as an entry's object is read
/// ([`body::Read::in_slot`]), all in the slot. A dictionary is skipped, which reads it
/// as reading it does.
fn object_in(data: &[u8], at: usize, slot: usize) -> Checked {
    let Some(mut r) = SlotReader::new(data, at, Slot::to(slot)) else {
        return Checked::Nothing;
    };
    let header = match r.header() {
        Header::Read(header) => header,
        Header::Cut => return Checked::Unfinished(None),
        Header::Absent => return Checked::Nothing,
    };
    let start = r.object_start();
    let (object, _) = r.object(
        |r| r.skip::<Dict<'_>>(false),
        |r| r.read_with_context::<Object<'_>>(&ReaderContext::dummy()),
    );
    match object {
        Some(_) => Checked::Object(header),
        None => Checked::Unfinished(Some((header, start))),
    }
}

// Try to manually parse the PDF to build an xref table and trailer dictionary.
//
// The objects the scan finds are the table's, the later of two that a damaged file
// numbers alike, as MuPDF rebuilds a table; an entry of the old table, `listed`, that
// leads to an object the scan does not find under the entry's number is kept. Such an
// object is the entry's, not the one its header names: where the scan finds it under
// its header's number, the scan takes the definition before it.
fn fallback(data: PdfData, password: &[u8], old: Old) -> Option<XRef> {
    warn!("xref table was invalid, trying to manually build xref table");
    let (xref_map, rebuilt, xref_input) = fallback_xref_map(&data, password, old);

    if let Some(xref_input) = xref_input {
        warn!("rebuild xref table with {} entries", xref_map.len());

        XRef::new(data.clone(), rebuilt, xref_map, xref_input, password).ok()
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

/// The identifier the header of each object of a rebuilt table names, where that is
/// not the entry's own: an entry of the old table kept for an object headed as
/// another. Found once, where the table is rebuilt, for every entry leading there.
type Headers = hash::Map<ObjectIdentifier, ObjectIdentifier>;

/// What reading a table finds besides its entries: where the objects of the file
/// are, which their slots end at, and, for a rebuilt table, the objects it lists
/// that cannot be read and the entries whose objects are headed as others.
#[derive(Debug, Clone)]
struct Rebuilt {
    layout: Layout,
    unreadable: Unreadable,
    headers: Headers,
}

impl Rebuilt {
    /// What a table read from the file finds: its objects are at its entries.
    fn sound(map: &XrefMap) -> Self {
        Self {
            layout: Layout::new(map),
            unreadable: Unreadable::default(),
            headers: Headers::default(),
        }
    }
}

fn fallback_xref_map<'a>(
    data: &'a PdfData,
    password: &[u8],
    old: Old,
) -> (XrefMap, Rebuilt, Option<XRefInput<'a>>) {
    let mut scan = Scan::new(data.as_ref());
    let mut listed = old.listed;
    let (read, unreadable) = read_unread(&mut scan, &old.unread);
    listed.extend(read);
    fallback_xref_map_inner(
        data,
        &mut scan,
        ReaderContext::dummy(),
        None,
        password,
        &listed,
        &unreadable,
    )
}

/// The entries of `unread`, sorted by offset, that lead to an object read to its end,
/// as reading the entry's object does, and those that lead to a header with no object
/// that can be read after it. Checking the table reads an object only up to the next
/// offset an entry names, which another's header may be at, in a string: the scan
/// reads the object on, from where the check found it starts, after a header and
/// white space read in the slot; one that does not start there, or whose header the
/// slot cuts, is not read, as reading the entry's object does not read it.
///
/// The headers and the white space before the objects read are in disjoint slots, so
/// the objects start at different places, each after a delimiter the slot holds: no
/// token the scan reads from one, but a string or a container, which it reads once
/// for all (`scan::Scan`), holds another's start.
fn read_unread(scan: &mut Scan<'_>, unread: &[Unfinished]) -> (Vec<Listed>, Vec<ObjectIdentifier>) {
    let mut listed = vec![];
    let mut unreadable = vec![];
    for &Unfinished { id, offset, header } in unread {
        match header {
            Some((header, Some(at))) if scan.object(at).is_some() => {
                listed.push(Listed { id, offset, header });
            }
            _ => unreadable.push(id),
        }
    }
    (listed, unreadable)
}

/// The table a scan of the file rebuilds, where objects in object streams are
/// decrypted by `decryptor`, `None` until the trailer is known: a document whose
/// trailer has /Encrypt is scanned again with its decryptor.
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
    scan: &mut Scan<'_>,
    mut dummy_ctx: ReaderContext<'a>,
    decryptor: Option<Arc<Decryptor>>,
    password: &[u8],
    listed: &[Listed],
    unread: &[ObjectIdentifier],
) -> (XrefMap, Rebuilt, Option<XRefInput<'a>>) {
    let mut xref_map = hash::Map::default();
    // Each object found, in the order of the file.
    let mut found = vec![];
    // Each header found with no object that can be read after it.
    let mut broken = vec![];
    let mut trailer_dicts = vec![];
    let mut root_ref = None;

    let mut r = Reader::new(data.as_ref());

    let mut last_obj_num = None;
    // The object streams found: the object, and where its header is.
    let mut object_streams = vec![];
    // The last header found with an object after it, where it is, and where its
    // object starts.
    let mut last_header = None;

    loop {
        let cur_pos = r.offset();

        // First try to check if we have an object identifier.
        if let Some(header) = rebuilt_header(&mut r, scan) {
            match header {
                Some((obj_id, Some(body))) => {
                    xref_map.insert(obj_id, EntryType::Normal(cur_pos));
                    found.push((obj_id, cur_pos));
                    last_obj_num = Some(obj_id);
                    last_header = Some((obj_id, cur_pos, body));
                    dummy_ctx.set_obj_number(obj_id);
                }
                Some((obj_id, None)) => broken.push(obj_id),
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
                if found.root || found.kind {
                    let mut dict_reader = Reader::new(&data.as_ref()[cur_pos..found.end]);
                    if let Some(dict) = dict_reader.read_with_context::<Dict<'_>>(&dummy_ctx) {
                        if found.root && dict.contains_key(ROOT) {
                            trailer_dicts.push(dict.clone());
                        }

                        if dict
                            .get::<Name<'_>>(TYPE)
                            .is_some_and(|n| n.as_str() == "Catalog")
                        {
                            root_ref = last_obj_num;
                        }

                        // An object stream is the body of its header's object, read
                        // once the scan has found every place, which its data may
                        // not reach past ([`XRef::object_stream_at`]).
                        if dict.get::<Name<'_>>(TYPE).as_deref() == Some(b"ObjStm")
                            && let Some((id, at, body)) = last_header
                            && body == cur_pos
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
                // until then.
                let old_pos = r.offset;
                r.forward_while(|b| !is_white_space_character(b));
                if r.offset == old_pos {
                    r.read_byte();
                }
            }
        }

        if r.at_end() {
            break;
        }
    }

    let layout = Layout::over(
        found
            .iter()
            .map(|(_, at)| *at)
            .chain(listed.iter().map(|entry| entry.offset))
            .collect(),
    );
    let decrypted = decryptor
        .clone()
        .unwrap_or_else(|| Arc::new(Decryptor::None));
    if !object_streams.is_empty() {
        // The objects each object stream holds, read through a table of the objects
        // found, which a stream's /Length may be.
        let probe = XRef::table(
            data.clone(),
            Rebuilt {
                layout: layout.clone(),
                unreadable: Unreadable::default(),
                headers: Headers::default(),
            },
            Arc::new(xref_map),
            decrypted.clone(),
        );
        let mut held = vec![];
        for (stream, at) in object_streams {
            if let Some(objects) = probe.object_stream_at(at, stream) {
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
        for (obj_num, stream, idx) in held {
            let id = ObjectIdentifier::new(obj_num as i32, 0);
            // If we already found an entry for that object number that was not
            // inside an object stream. Somewhat arbitrary and maybe
            // we can do better, but that seems to work for the current
            // set of tests.
            if xref_map
                .get(&id)
                .is_none_or(|e| !matches!(e, &EntryType::Normal(_)))
            {
                xref_map.insert(
                    id,
                    EntryType::ObjStream(stream.obj_number as u32, idx as u32),
                );
            }
        }
    }

    let headers = keep_listed(&mut xref_map, &found, listed);
    let unreadable: Unreadable = unread
        .iter()
        .chain(&broken)
        .filter(|id| !xref_map.contains_key(id))
        .copied()
        .collect();
    let mut rebuilt = Rebuilt {
        layout,
        unreadable,
        headers,
    };

    // Try to choose the right trailer dict by doing basic validation: the last whose
    // /Root is a dictionary with /Pages, read as the rebuilt table reads it.
    let has_pages = |dict: &Dict<'_>| -> bool { dict.contains_key(PAGES) };
    let table = XRef::table(data.clone(), rebuilt.clone(), Arc::new(xref_map), decrypted);
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
    let mut xref_map = table.into_map();

    let has_encryption = trailer_dict
        .as_ref()
        .is_some_and(|t| t.contains_key(ENCRYPT));

    if has_encryption && decryptor.is_none() {
        // The problem is that in this case, we have used a dummy reader context which does not have
        // a decryptor. Therefore, we were unable to decrypt any of the object streams and missed
        // all objects that are inside of such a stream. Therefore, we need to redo the process
        // using a `ReaderContext` that does have the ability to decrypt.
        if let Ok(xref) = XRef::new(
            data.clone(),
            rebuilt.clone(),
            xref_map.clone(),
            XRefInput::TrailerDictData(trailer_dict.as_ref().map(|d| d.data()).unwrap()),
            password,
        ) {
            let ctx = ReaderContext::new(&xref, false);
            let decryptor = xref.decryptor();
            let (patched_map, patched, _) =
                fallback_xref_map_inner(data, scan, ctx, Some(decryptor), password, listed, unread);
            xref_map = patched_map;
            rebuilt = patched;
        }
    }

    let input = if let Some(trailer_dict_data) = trailer_dict.map(|d| d.data()) {
        Some(XRefInput::TrailerDictData(trailer_dict_data))
    } else {
        root_ref.map(XRefInput::RootRef)
    };
    (xref_map, rebuilt, input)
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

/// Adds to the rebuilt table `xref_map` the entries of the old table, `listed`, whose
/// number the scan did not find, and returns those whose header names another
/// object, with that object. The object such an entry leads to is the entry's:
/// where its header names another number, the scan takes for that number the last
/// definition no such entry leads to, unless the old table, too, lists that number
/// at the entry's offset.
fn keep_listed(
    xref_map: &mut XrefMap,
    found: &[(ObjectIdentifier, usize)],
    listed: &[Listed],
) -> Headers {
    let kept: Vec<Listed> = listed
        .iter()
        .filter(|entry| !xref_map.contains_key(&entry.id))
        .copied()
        .collect();
    let headed: BTreeSet<usize> = listed
        .iter()
        .filter(|entry| entry.header == entry.id)
        .map(|entry| entry.offset)
        .collect();
    let claimed: BTreeSet<usize> = kept
        .iter()
        .filter(|entry| entry.header != entry.id && !headed.contains(&entry.offset))
        .map(|entry| entry.offset)
        .collect();
    let mut taken: BTreeSet<ObjectIdentifier> = xref_map
        .iter()
        .filter(|(_, entry)| matches!(entry, EntryType::Normal(at) if claimed.contains(at)))
        .map(|(id, _)| *id)
        .collect();
    for id in &taken {
        xref_map.remove(id);
    }
    for (id, offset) in found.iter().rev() {
        if !claimed.contains(offset) && taken.remove(id) {
            xref_map.insert(*id, EntryType::Normal(*offset));
        }
    }
    let mut headers = Headers::default();
    for entry in kept {
        xref_map.insert(entry.id, EntryType::Normal(entry.offset));
        if entry.header != entry.id {
            headers.insert(entry.id, entry.header);
        }
    }
    headers
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
            unreadable: Arc::new(rebuilt.unreadable),
            headers: Arc::new(rebuilt.headers),
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

    fn new(
        data: PdfData,
        rebuilt: Rebuilt,
        xref_map: XrefMap,
        input: XRefInput<'_>,
        password: &[u8],
    ) -> Result<Self, XRefError> {
        // This is a bit hacky, but the problem is we can't read the resolved trailer dictionary
        // before we actually created the xref struct. So we first create it using dummy data
        // and then populate the data.
        let mut xref = Self::table(data, rebuilt, Arc::new(xref_map), Arc::new(Decryptor::None));

        // We read the trailer twice, once to determine the encryption used and then a second
        // time to resolve the catalog dictionary, etc. This allows us to support catalog dictionaries
        // that are stored in an encrypted object stream.

        let decryptor = {
            match input {
                XRefInput::TrailerDictData(trailer_dict_data) => {
                    let mut r = Reader::new(trailer_dict_data);

                    let trailer_dict = r
                        .read_with_context::<Dict<'_>>(&ReaderContext::new(&xref, false))
                        .ok_or(XRefError::Unknown)?;

                    get_decryptor(&trailer_dict, password)?
                }
                XRefInput::RootRef(_) => Decryptor::None,
            }
        };

        match &mut xref.0 {
            Inner::Dummy => unreachable!(),
            Inner::Some(r) => {
                let mutable = Arc::make_mut(r);
                mutable.decryptor = Arc::new(decryptor.clone());
            }
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

        match &mut xref.0 {
            Inner::Dummy => unreachable!(),
            Inner::Some(r) => {
                let mutable = Arc::make_mut(r);
                mutable.trailer_data = trailer_data;
                mutable.decryptor = Arc::new(decryptor);
                mutable.has_ocgs = has_ocgs;
                mutable.metadata = Arc::new(metadata);
                // What was read before the decryptor was known is not kept.
                mutable.shapes = Arc::default();
            }
        }

        Ok(xref)
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
                        };

                        (*id, offset)
                    })
                    .collect::<Vec<_>>();

                // Try to yield in the order the objects appeared in the
                // PDF.
                elements.sort_by_key(|element| element.1);

                let mut iter = elements.into_iter();
                let ctx = ReaderContext::new(self, false);

                iter::from_fn(move || {
                    for next in iter.by_ref() {
                        if let Some(obj) = self.get_parsed(next.0, &ctx) {
                            return Some(obj);
                        } else {
                            // Skip invalid objects.
                            continue;
                        }
                    }

                    None
                })
            }
        }
    }

    #[inline]
    /// Whether the document is encrypted.
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
            Inner::Some(r) => r.map.contains_key(&id) || r.unreadable.contains(&id),
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
                None => self.read(id, &ctx, Taken::Either).and(None),
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
            Inner::Some(r) => r.unreadable.first().copied(),
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
    pub(crate) fn apart<T>(&self, f: impl FnOnce() -> T) -> (T, Lost) {
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
        self.read(id, ctx, taken)?.cast()
    }

    /// Object `id`, read from `ctx` as [`Self::get_with`] reads any object, but a
    /// stream whose data cannot be read is its dictionary, as each place this is
    /// used reads it as a dictionary, if not as another kind of object.
    pub(crate) fn get_parsed<'a>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Option<Object<'a>> {
        self.read(id, ctx, Taken::Either)
    }

    /// The object entry `id` leads to, read from `ctx`, a stream taken as `taken`
    /// says; `None` where the table does not list it, where `ctx`'s chain rejects its
    /// body or what reading it needs as a cycle ([`Link`]), or where it is lost, which
    /// the computation being watched is told: every read of an object goes through
    /// here.
    fn read<'a>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
        taken: Taken,
    ) -> Option<Object<'a>> {
        let (repr, entry, entered) = self.entry(id, ctx)?;
        match self.body(repr, id, entry, ctx, &entered, taken) {
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
    fn body<'a>(
        &'a self,
        repr: &'a SomeRepr,
        id: ObjectIdentifier,
        entry: EntryType,
        ctx: &ReaderContext<'a>,
        entered: &ReaderContext<'a>,
        taken: Taken,
    ) -> Result<Object<'a>, Unread> {
        let (place, data, slot) = self.place(repr, id, entry, entered)?;
        let link = place.link();
        if ctx.rejects(&link) {
            warn!("cycle detected in indirect object: {id:?}");

            return Err(Unread::Rejected);
        }
        let mut entered = entered.clone();
        entered.parent_chain_push(link);
        self.at_place(repr, id, entry, (place, data, slot), ctx, &entered, taken)
    }

    /// The object the body at `place`, in `data`, whose slot ends at `slot`, holds,
    /// read for entry `id`, at `entry`, in `entered`, the context of the entry from
    /// `ctx` whose chain holds the body: read once for every entry leading there and
    /// every read.
    #[allow(clippy::too_many_arguments)]
    fn at_place<'a>(
        &'a self,
        repr: &'a SomeRepr,
        id: ObjectIdentifier,
        entry: EntryType,
        (place, data, slot): (Place, &'a [u8], Slot),
        ctx: &ReaderContext<'a>,
        entered: &ReaderContext<'a>,
        taken: Taken,
    ) -> Result<Object<'a>, Unread> {
        let kept = repr.shapes.get().get(&place).cloned();
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
                repr.shapes.get().entry(place).or_insert(shape).clone()
            }
        };
        match shape {
            body::Shape::Unreadable => Err(Unread::Lost(id)),
            body::Shape::Value(node) => Ok(node.object(data, entered)),
            body::Shape::Stream {
                dict: shape,
                end,
                data: kept,
            } => {
                let dict = Dict::parsed(data, shape.clone(), entered);
                if taken == Taken::Dict {
                    return Ok(Object::Dict(dict));
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
                                let dict = Dict::parsed(data, shape, &entered);
                                repr.stream_data(data, place, slot, end, &dict)
                            })
                        });
                        if shared
                            && let Some(body::Shape::Stream { data: kept, .. }) =
                                repr.shapes.get().get_mut(&place)
                        {
                            kept.get_or_insert((found, lost));
                        }
                        found
                    }
                };
                match (found, taken) {
                    (Some((from, _, cut)), Taken::Contained) => {
                        Ok(Object::Stream(Stream::new(&data[from..cut], dict)))
                    }
                    (Some((from, to, _)), _) => {
                        Ok(Object::Stream(Stream::new(&data[from..to], dict)))
                    }
                    (None, Taken::Either) => Ok(Object::Dict(dict)),
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
            EntryType::ObjStream(stream, index) => {
                // Generation number is implicitly 0.
                let stream = ObjectIdentifier::new(stream as i32, 0);
                if stream == id {
                    warn!("object stream holding itself");

                    return Err(Unread::Lost(id));
                }
                let (kept, objects) = self.object_stream(repr, stream, entered)?;
                let at = objects.offset(id, index).ok_or(Unread::Lost(id))?;
                let place = Place {
                    source: Source::Stream(kept),
                    at,
                    decryption,
                };
                Ok((place, objects.data(), objects.slot(at)))
            }
        }
    }

    /// Object stream `stream`, read from `ctx`, decoded once for every entry leading
    /// to its place ([`Self::decoded`]), and where it is kept. A stream the table does
    /// not list, or cannot read, is lost, with the objects in it.
    fn object_stream<'a>(
        &'a self,
        repr: &'a SomeRepr,
        stream: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Result<(usize, &'a ObjectStream), Unread> {
        let Some(&entry) = repr.map.get(&stream) else {
            return Err(Unread::Lost(stream));
        };
        let mut entered = Self::entered(ctx, stream, repr, entry);
        // A stream in another is found by reading that one, which the chain stops
        // where it is finding the stream already.
        if matches!(entry, EntryType::ObjStream(..)) {
            let locating = Link::Locating(stream);
            if ctx.rejects(&locating) {
                return Err(Unread::Rejected);
            }
            entered.parent_chain_push(locating);
        }
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
            let read = |taken| self.at_place(repr, stream, entry, place, ctx, &entered, taken);
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
        let entry = EntryType::Normal(at);
        let entered = Self::entered(&ctx, header, repr, entry);
        let place = self.place(repr, header, entry, &entered).ok()?;
        let (_, objects) = self.decoded(repr, header, entry, place, &ctx).ok()?;
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
            if repr.unreadable.contains(&id) {
                self.note(Some(id));
            }
            return None;
        };

        Some((repr, entry, Self::entered(ctx, id, repr, entry)))
    }

    /// The context entry `id`, at `entry`, is read in from `ctx`, but for the link to
    /// what it reads, which the caller adds to the chain once it knows it ([`Link`]).
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
        if let Some(header) = repr.headers.get(&id) {
            ctx.set_decryption(*header);
        }
        ctx.set_in_content_stream(false);
        ctx.set_in_object_stream(matches!(entry, EntryType::ObjStream(..)));
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
    ends: Arc<Ends>,
}

impl Layout {
    /// The layout of the file whose objects `map` puts at its offsets.
    fn new(map: &XrefMap) -> Self {
        Self::over(
            map.values()
                .filter_map(|entry| match entry {
                    EntryType::Normal(at) => Some(*at),
                    EntryType::ObjStream(..) => None,
                })
                .collect(),
        )
    }

    /// The layout of the file whose objects are at `starts`.
    fn over(mut starts: Vec<usize>) -> Self {
        starts.sort_unstable();
        starts.dedup();
        Self {
            starts: starts.into(),
            ends: Arc::default(),
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
            let from = r.offset();
            let near = &data[from..(from + NEAR).min(data.len())];
            let white = near
                .iter()
                .take_while(|b| is_white_space_character(**b))
                .count();
            if white < NEAR {
                r.jump(from + white);
                return r.forward_tag(b"endstream");
            }
            let (stop, trimmed) = self.endstream_after(data, from)?;
            (trimmed <= from).then(|| r.jump(stop + b"endstream".len()))
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
                let (_, trimmed) = self.endstream_after(data, from)?;
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

    /// The first `endstream` at or after `from` of the file, `data`, and where the
    /// data before it end, without the white space before it.
    fn endstream_after(&self, data: &[u8], from: usize) -> Option<(usize, usize)> {
        let ends = self.ends.get_or_init(|| {
            let mut ends = vec![];
            let mut at = 0;
            while let Some(found) = object::stream::searched(&data[at..], b"endstream") {
                // The white space before it, up to the end of the one before at most,
                // which ends in none.
                let stop = at + found;
                ends.push((
                    stop,
                    object::stream::trim_trailing_ascii_whitespace(&data[..stop]),
                ));
                at = stop + b"endstream".len();
            }
            crate::work::read_bodies(data.len());
            ends
        });
        ends.get(ends.partition_point(|(at, _)| *at < from))
            .copied()
    }
}

/// Where each `endstream` of a file is, and where the data before it end ([`Layout`]).
type Ends = OnceLock<Vec<(usize, usize)>>;

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
    /// The objects a rebuilt table lists that cannot be read.
    unreadable: Arc<Unreadable>,
    /// The entries of a rebuilt table whose objects are headed as others.
    headers: Arc<Headers>,
    /// What the body at each place an entry leads to holds, read once for every
    /// read of an entry leading there ([`XRef::get_with`]).
    shapes: Arc<Mutex<hash::Map<Place, body::Shape>>>,
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
struct Sections(alloc::collections::BTreeMap<usize, usize>);

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
    /// end of the data, which goes on unknown where they are cut.
    fn slot(&self, at: usize) -> Slot {
        let next = self.starts.partition_point(|start| *start <= at);
        let end = self
            .starts
            .get(next)
            .map_or(self.data.len(), |next| *next)
            .min(self.data.len());
        Slot {
            end,
            open: self.cut && end == self.data.len(),
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
            .map(|t| t.to_vec()),
        author: info_dict
            .get::<object::String<'_>>(AUTHOR)
            .map(|t| t.to_vec()),
        subject: info_dict
            .get::<object::String<'_>>(SUBJECT)
            .map(|t| t.to_vec()),
        keywords: info_dict
            .get::<object::String<'_>>(KEYWORDS)
            .map(|t| t.to_vec()),
        creator: info_dict
            .get::<object::String<'_>>(CREATOR)
            .map(|t| t.to_vec()),
        producer: info_dict
            .get::<object::String<'_>>(PRODUCER)
            .map(|t| t.to_vec()),
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
        let families: [Family; 23] = [
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
            let [small, large] = [1000, 2000].map(|n| {
                let file = file(n);
                let work = work(&file);
                assert!(
                    work <= 13 * file.len(),
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

    // A page holding a name whose escapes are not hex, which reading a dictionary
    // skips, is read whether its table is sound or rebuilt, and so is one holding such
    // a name in a dictionary in it: checking the table and rebuilding it read each
    // object as reading it does, and a dictionary is read alike wherever it is.
    #[test]
    fn a_page_holding_a_name_that_is_no_key_is_read() {
        let content = stream("", BLUE);
        for page in [
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R /Broken#zz >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R \
             /Resources << /Broken#zz >> >>",
            "<< /Type /Page /Parent 2 0 R /Broken#zz /MediaBox [0 0 600 700] /Contents 4 0 R >>",
        ] {
            for broken in [false, true] {
                let file = file_of(&[CATALOG, TREE, page, &content], broken);
                assert_eq!(
                    one_page(file),
                    (600.0, 700.0, BLUE.as_bytes().to_vec()),
                    "{page}, rebuilt: {broken}"
                );
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
    // offset the entry or the stream's header gives, reads its body at most once, and
    // a read that finds a body that cannot be read as what it asks for tells the
    // computation being watched that the object is lost.
    #[test]
    fn every_read_of_an_entry_reads_its_body_once_and_tells_what_it_lost() {
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
            assert!(most <= 1, "seed {seed}: a body read {most} times");
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
            assert!(most <= 1, "seed {seed}: a body read {most} times");
        }
    }
}
