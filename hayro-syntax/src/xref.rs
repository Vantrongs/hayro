//! Reading and querying the xref table of a PDF file.

use crate::crypto::reader::DecryptedReader;
use crate::crypto::{DecryptionError, DecryptionTarget, Decryptor, get};
use crate::data::{Data, Decoded};
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
use crate::reader::{Readable, ReaderContext, ReaderExt};
use crate::sync::{Arc, FxHashMap, Mutex, MutexExt, OnceLock};
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

use body::{Place, Source, Taken, Unread};
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

#[derive(Debug, Copy, Clone)]
pub(crate) enum XRefError {
    Unknown,
    Encryption(DecryptionError),
}

/// The table of the PDF: the one its last `startxref` leads to, or, where that cannot
/// be read or has an entry that does not lead to its object, one rebuilt by a scan
/// of the file.
pub(crate) fn open(data: PdfData, password: &[u8]) -> Result<XRef, XRefError> {
    let mut xref_map = FxHashMap::default();
    let trailer = find_last_xref_pos(data.as_ref())
        .and_then(|pos| populate_xref_impl(data.as_ref(), pos, &mut xref_map));
    let Some(trailer) = trailer else {
        return fallback(data, password, Old::default()).ok_or(XRefError::Unknown);
    };

    match check(&mut xref_map, data.as_ref()) {
        Ok(()) => match XRef::new(
            data.clone(),
            xref_map,
            Rebuilt::default(),
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
    /// The others, each by offset: the scan reads their objects to their ends.
    unread: Vec<(usize, ObjectIdentifier)>,
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
    /// The work opening files has done on this thread to check their tables and to
    /// scan the files whose tables are rebuilt: the bytes read, and the blocks, tree
    /// nodes and outcomes a scan looks at (`scan::Scan`). The tests bound it to show
    /// that opening a file reads each part of it a bounded number of times.
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
// So each object is read once however many entries name it, and no further than the
// next, which reads each byte of the file once. MuPDF, too, rebuilds a table with an
// entry past the end of the file or in an object stream the table does not list as an
// object (`check_xref_entry_offsets` in MuPDF's `pdf-xref.c`); an entry at offset 0 is
// free.
fn check(xref_map: &mut XrefMap, data: &[u8]) -> Result<(), Old> {
    xref_map.retain(|_, entry| *entry != EntryType::Normal(0));
    let mut entries = vec![];
    let mut streams = vec![];
    for (id, entry) in xref_map.iter() {
        match *entry {
            EntryType::Normal(offset) => entries.push((offset, *id)),
            EntryType::ObjStream(stream, _) => streams.push(stream),
        }
    }
    entries.sort_unstable_by_key(|(offset, _)| *offset);

    let mut sound = true;
    let mut old = Old::default();
    // The entries at each offset, and the next offset.
    let mut rest = entries.as_slice();
    while let Some(&(offset, _)) = rest.first() {
        let at = rest.partition_point(|(other, _)| *other == offset);
        let end = rest.get(at).map_or(data.len(), |(next, _)| *next);
        let header = data.get(offset..end).and_then(object_in);
        for (_, id) in &rest[..at] {
            match header {
                Some(header) => {
                    sound &= header == *id;
                    old.listed.push(Listed {
                        id: *id,
                        offset,
                        header,
                    });
                }
                None => {
                    sound = false;
                    old.unread.push((offset, *id));
                }
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

/// The identifier in the header of the object `data` starts with, if the object ends
/// in `data`: if reading it as an entry's object is read ([`read_object`]) reads it
/// there. A dictionary is skipped, which reads it as reading it does.
fn object_in(data: &[u8]) -> Option<ObjectIdentifier> {
    let header = Reader::new(data).read_without_context::<ObjectIdentifier>()?;
    let mut r = Reader::new(data);
    let object = read_object(&mut r, true, &ReaderContext::dummy(), |r| {
        r.skip::<Dict<'_>>(false)
    });
    #[cfg(test)]
    opened(if object.is_some() {
        r.offset()
    } else {
        data.len()
    });
    object.map(|_| header)
}

/// The object `r` is at, after its header where `header`, and where it starts, as an
/// entry's object is read: a dictionary, read by `dict`, or another object. This is
/// how every read of an entry reads an object's body (`body::Shape::read`), keeping
/// the dictionary, and how the table's check finds that an entry leads to one, skipping
/// it: a dictionary is skipped and read by one reading.
fn read_object<'a, D>(
    r: &mut Reader<'a>,
    header: bool,
    ctx: &ReaderContext<'a>,
    dict: impl FnOnce(&mut Reader<'a>) -> Option<D>,
) -> Option<(usize, Result<D, Object<'a>>)> {
    if header {
        r.skip::<ObjectIdentifier>(false)?;
    }
    r.skip_white_spaces_and_comments();
    let at = r.offset();
    if r.peek_bytes(2) == Some(b"<<") {
        return Some((at, Ok(dict(r)?)));
    }
    Some((at, Err(r.read_with_context::<Object<'a>>(ctx)?)))
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

        XRef::new(data.clone(), xref_map, rebuilt, xref_input, password).ok()
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
type Headers = FxHashMap<ObjectIdentifier, ObjectIdentifier>;

/// What a rebuild finds besides the table: the objects it lists that cannot be
/// read, and the entries whose objects are headed as others.
#[derive(Debug, Default, Clone)]
struct Rebuilt {
    unreadable: Unreadable,
    headers: Headers,
}

fn fallback_xref_map<'a>(
    data: &'a PdfData,
    password: &[u8],
    old: Old,
) -> (XrefMap, Rebuilt, Option<XRefInput<'a>>) {
    let mut scan = Scan::new(data.as_ref());
    let mut listed = old.listed;
    let (read, unreadable) = read_unread(data.as_ref(), &mut scan, &old.unread);
    listed.extend(read);
    fallback_xref_map_inner(
        data,
        &mut scan,
        ReaderContext::dummy(),
        true,
        password,
        &listed,
        &unreadable,
    )
}

/// The entries of `unread`, sorted by offset, that lead to an object read to its end,
/// as reading the entry's object does, and those that lead to a header with no object
/// that can be read after it. Checking the table reads an object only up to the next
/// offset an entry names, which another's header may be at, in a string.
fn read_unread(
    data: &[u8],
    scan: &mut Scan<'_>,
    unread: &[(usize, ObjectIdentifier)],
) -> (Vec<Listed>, Vec<ObjectIdentifier>) {
    let mut listed = vec![];
    let mut unreadable = vec![];
    let mut rest = unread;
    while let Some(&(offset, _)) = rest.first() {
        let at = rest.partition_point(|(other, _)| *other == offset);
        let mut r = Reader::new(data);
        let header = data.get(offset..).and_then(|_| {
            r.jump(offset);
            let header = r.read_without_context::<ObjectIdentifier>()?;
            r.skip_white_spaces_and_comments();
            opened(r.offset() - offset);
            Some((header, scan.object(r.offset()).is_some()))
        });
        match header {
            Some((header, true)) => {
                listed.extend(
                    rest[..at]
                        .iter()
                        .map(|&(offset, id)| Listed { id, offset, header }),
                );
            }
            Some((_, false)) => unreadable.extend(rest[..at].iter().map(|&(_, id)| id)),
            None => {}
        }
        rest = &rest[at..];
    }
    (listed, unreadable)
}

fn fallback_xref_map_inner<'a>(
    data: &'a PdfData,
    scan: &mut Scan<'_>,
    mut dummy_ctx: ReaderContext<'a>,
    recurse: bool,
    password: &[u8],
    listed: &[Listed],
    unread: &[ObjectIdentifier],
) -> (XrefMap, Rebuilt, Option<XRefInput<'a>>) {
    let mut xref_map = FxHashMap::default();
    // Each object found, in the order of the file.
    let mut found = vec![];
    // Each header found with no object that can be read after it.
    let mut broken = vec![];
    let mut trailer_dicts = vec![];
    let mut root_ref = None;

    let mut r = Reader::new(data.as_ref());

    let mut last_obj_num = None;

    loop {
        let cur_pos = r.offset();

        let mut old_r = r.clone();

        // First try to check if we have an object identifier.
        if let Some(header) = rebuilt_header(&mut r, scan) {
            match header {
                Some((obj_id, true)) => {
                    xref_map.insert(obj_id, EntryType::Normal(cur_pos));
                    found.push((obj_id, cur_pos));
                    last_obj_num = Some(obj_id);
                    dummy_ctx.set_obj_number(obj_id);
                }
                Some((obj_id, false)) => broken.push(obj_id),
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
                    opened(found.end - cur_pos);
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

                        // Only an object stream is read as a stream: reading another
                        // dictionary as one searches the rest of the file for one.
                        if dict.get::<Name<'_>>(TYPE).as_deref() == Some(b"ObjStm")
                            && let Some(stream) = old_r.read::<Stream<'_>>(&dummy_ctx)
                            && let Some(last_obj_num) = last_obj_num
                            && let Some(obj_stream) = ObjectStream::new(&stream)
                        {
                            for (idx, (obj_num, _)) in obj_stream.offsets.iter().enumerate() {
                                let id = ObjectIdentifier::new(*obj_num as i32, 0);
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
                                        EntryType::ObjStream(
                                            last_obj_num.obj_number as u32,
                                            idx as u32,
                                        ),
                                    );
                                }
                            }
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

    let headers = keep_listed(&mut xref_map, &found, listed);
    let unreadable: Unreadable = unread
        .iter()
        .chain(&broken)
        .filter(|id| !xref_map.contains_key(id))
        .copied()
        .collect();
    let mut rebuilt = Rebuilt {
        unreadable,
        headers,
    };

    // Try to choose the right trailer dict by doing basic validation: the last whose
    // /Root is a dictionary with /Pages. Each catalog the trailers name is read once,
    // however many name it or entries lead to it.
    let has_pages = |dict: &Dict<'_>| -> bool { dict.contains_key(PAGES) };
    let catalog_has_pages = |id: ObjectIdentifier| -> bool {
        match xref_map.get(&id) {
            Some(EntryType::Normal(offset)) => {
                let mut reader = Reader::new(data.as_ref());
                reader.jump(*offset);
                let mut ctx = dummy_ctx.clone();
                ctx.set_obj_number(id);
                let catalog = reader.read_with_context::<IndirectObject<Dict<'_>>>(&ctx);
                opened(reader.offset() - offset);
                catalog.is_some_and(|catalog| has_pages(&catalog.get()))
            }
            Some(EntryType::ObjStream(obj_num, idx)) => {
                let stream_id = ObjectIdentifier::new(*obj_num as i32, 0);
                let Some(EntryType::Normal(offset)) = xref_map.get(&stream_id) else {
                    return false;
                };
                let mut reader = Reader::new(data.as_ref());
                reader.jump(*offset);
                let mut ctx = dummy_ctx.clone();
                ctx.set_obj_number(stream_id);
                let stream = reader.read_with_context::<IndirectObject<Stream<'_>>>(&ctx);
                opened(reader.offset() - offset);
                stream
                    .and_then(|stream| ObjectStream::new(&stream.get()))
                    .is_some_and(|objects| {
                        let Some(at) = objects.offset(id, *idx) else {
                            return false;
                        };
                        let mut r = Reader::new(objects.data());
                        r.jump(at);
                        r.skip_white_spaces_and_comments();
                        r.read_with_context::<Dict<'_>>(&dummy_ctx)
                            .is_some_and(|catalog| has_pages(&catalog))
                    })
            }
            None => false,
        }
    };
    let mut catalogs = FxHashMap::default();
    let mut trailer_dict = None;
    for dict in trailer_dicts {
        let valid = match dict.get_raw::<Dict<'_>>(ROOT) {
            Some(MaybeRef::Ref(r)) => {
                let id = ObjectIdentifier::from(r);
                // An object at an offset is the same dictionary under any entry.
                let read = match xref_map.get(&id) {
                    Some(EntryType::Normal(offset)) => (Some(*offset), None),
                    _ => (None, Some(id)),
                };
                *catalogs
                    .entry(read)
                    .or_insert_with(|| catalog_has_pages(id))
            }
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

    if has_encryption && recurse {
        // The problem is that in this case, we have used a dummy reader context which does not have
        // a decryptor. Therefore, we were unable to decrypt any of the object streams and missed
        // all objects that are inside of such a stream. Therefore, we need to redo the process
        // using a `ReaderContext` that does have the ability to decrypt.
        if let Ok(xref) = XRef::new(
            data.clone(),
            xref_map.clone(),
            rebuilt.clone(),
            XRefInput::TrailerDictData(trailer_dict.as_ref().map(|d| d.data()).unwrap()),
            password,
        ) {
            let ctx = ReaderContext::new(&xref, false);
            let (patched_map, patched, _) =
                fallback_xref_map_inner(data, scan, ctx, false, password, listed, unread);
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
/// else the header there, if any, and whether an object follows it, read to its end
/// however far that is, as reading it does; `r` after the header. The scan reads each
/// part of the file a bounded number of times however many objects it holds.
fn rebuilt_header(
    r: &mut Reader<'_>,
    scan: &mut Scan<'_>,
) -> Option<Option<(ObjectIdentifier, bool)>> {
    if !r.peek_byte().is_some_and(object::starts_number) {
        return None;
    }
    let Some(id) = scan_header(r) else {
        return Some(None);
    };
    let mut body = r.clone();
    body.skip_white_spaces_and_comments();
    opened(body.offset() - r.offset());
    Some(Some((id, scan.object(body.offset()).is_some())))
}

/// The header `r` starts with, `N G obj`, and `r` after it.
fn scan_header(r: &mut Reader<'_>) -> Option<ObjectIdentifier> {
    let mut header = r.clone();
    let id = header.read_without_context::<ObjectIdentifier>();
    #[cfg(test)]
    opened(match id {
        Some(_) => header.offset() - r.offset(),
        // A header that is not one reads two numbers, the white space and comments
        // after them, and three bytes.
        None => {
            let mut read = r.clone();
            let _ = read.read_without_context::<i32>();
            read.skip_white_spaces_and_comments();
            let _ = read.read_without_context::<i32>();
            read.skip_white_spaces_and_comments();
            read.offset() + 3 - r.offset()
        }
    });
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
    /// The entries of object streams read on this thread, which the tests bound to
    /// show that each object stream is read once.
    pub(crate) static OBJECT_STREAM_ENTRIES: core::cell::Cell<usize> =
        const { core::cell::Cell::new(0) };
    /// While `Some`, how often the body of each object has been read on this thread,
    /// by the entry a read of it was made for, which the tests bound to show that
    /// loading the pages reads each object once.
    pub(crate) static RESOLVED: core::cell::RefCell<Option<FxHashMap<ObjectIdentifier, usize>>> =
        const { core::cell::RefCell::new(None) };
}

/// An xref table.
#[derive(Debug, Clone)]
pub struct XRef(Inner);

impl XRef {
    fn new(
        data: PdfData,
        xref_map: XrefMap,
        rebuilt: Rebuilt,
        input: XRefInput<'_>,
        password: &[u8],
    ) -> Result<Self, XRefError> {
        // This is a bit hacky, but the problem is we can't read the resolved trailer dictionary
        // before we actually created the xref struct. So we first create it using dummy data
        // and then populate the data.
        let trailer_data = TrailerData::dummy();

        let mut xref = Self(Inner::Some(Arc::new(SomeRepr {
            document: DocumentId::new(),
            data: Arc::new(Data::new(data)),
            map: Arc::new(xref_map),
            unreadable: Arc::new(rebuilt.unreadable),
            headers: Arc::new(rebuilt.headers),
            shapes: Arc::default(),
            decryptor: Arc::new(Decryptor::None),
            has_ocgs: false,
            metadata: Arc::new(Metadata::default()),
            trailer_data,
        })));

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
    /// says; `None` where the table does not list it, where `ctx`'s chain rejects it
    /// or what reading it needs as a cycle, or where it is lost, which the
    /// computation being watched is told: every read of an object goes through here.
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
    /// for every entry leading to the same place and every read.
    fn body<'a>(
        &'a self,
        repr: &'a SomeRepr,
        id: ObjectIdentifier,
        entry: EntryType,
        ctx: &ReaderContext<'a>,
        entered: &ReaderContext<'a>,
        taken: Taken,
    ) -> Result<Object<'a>, Unread> {
        let (place, data) = self.place(repr, id, entry, entered)?;
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
                let shape = body::Shape::read(data, place, entered);
                repr.shapes.get().entry(place).or_insert(shape).clone()
            }
        };
        match shape {
            body::Shape::Unreadable => Err(Unread::Lost(id)),
            body::Shape::Value(value) => Ok(value.object(data, entered)),
            body::Shape::Stream {
                dict: shape,
                end,
                data: kept,
            } => {
                let dict = shape.object(data, entered);
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
                                let entered = Self::entered(ctx, id, repr, entry);
                                body::stream_data(data, place, end, shape.object(data, &entered))
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
                    (Some((from, to)), _) => Ok(Object::Stream(Stream::new(&data[from..to], dict))),
                    (None, Taken::Either) => Ok(Object::Dict(dict)),
                    (None, _) => Err(Unread::Lost(id)),
                }
            }
        }
    }

    /// Where entry `id`, at `entry`, leads, read in `entered`, and the bytes the place
    /// is in. This reads nothing of the file: the body is read at the place
    /// ([`body::Shape::read`]).
    fn place<'a>(
        &'a self,
        repr: &'a SomeRepr,
        id: ObjectIdentifier,
        entry: EntryType,
        entered: &ReaderContext<'a>,
    ) -> Result<(Place, &'a [u8]), Unread> {
        let decryption = entered.decryption();
        match entry {
            EntryType::Normal(at) => Ok((
                Place {
                    source: Source::File,
                    at,
                    decryption,
                },
                repr.data.get().as_ref(),
            )),
            EntryType::ObjStream(stream, index) => {
                // Generation number is implicitly 0.
                let stream = ObjectIdentifier::new(stream as i32, 0);
                if stream == id {
                    warn!("object stream holding itself");

                    return Err(Unread::Lost(id));
                }
                let (kept, objects) = match repr.data.get_with(stream, entered) {
                    Decoded::Stream(kept, objects) => (kept, objects),
                    Decoded::Lost(lost) => return Err(Unread::Lost(lost)),
                    Decoded::Rejected => return Err(Unread::Rejected),
                };
                let at = objects.offset(id, index).ok_or(Unread::Lost(id))?;
                let place = Place {
                    source: Source::Stream(kept),
                    at,
                    decryption,
                };
                Ok((place, repr.data.decoded(kept)))
            }
        }
    }

    /// The entry of object `id`, and the context to read it in, reached from `ctx`:
    /// what is read from it is cached and checked for cycles as `id`, the entry it is
    /// reached through, whatever its header says. This is the only place that names the
    /// object a context reads. An object the table lists but that cannot be read has
    /// no entry, and is lost ([`Self::watch`]).
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

        if ctx.rejects(&id) {
            warn!("cycle detected in indirect object: {id:?}");

            return None;
        }

        Some((repr, entry, Self::entered(ctx, id, repr, entry)))
    }

    /// The context entry `id`, at `entry`, is read in from `ctx`.
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
        ctx.parent_chain_push(id);
        ctx.set_in_content_stream(false);
        ctx.set_in_object_stream(matches!(entry, EntryType::ObjStream(..)));
        ctx
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
    let pos = findr_needle(data, needle)?;
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

type XrefMap = FxHashMap<ObjectIdentifier, EntryType>;

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
    /// Never changed once read: what is read through it is read from one table.
    map: Arc<XrefMap>,
    /// The objects a rebuilt table lists that cannot be read.
    unreadable: Arc<Unreadable>,
    /// The entries of a rebuilt table whose objects are headed as others.
    headers: Arc<Headers>,
    /// What the body at each place an entry leads to holds, read once for every
    /// read of an entry leading there ([`XRef::get_with`]).
    shapes: Arc<Mutex<FxHashMap<Place, body::Shape>>>,
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

fn populate_xref_impl<'a>(data: &'a [u8], pos: usize, xref_map: &mut XrefMap) -> Option<&'a [u8]> {
    let mut visited = BTreeSet::new();
    populate_xref_impl_inner(data, pos, xref_map, &mut visited)
}

/// Maximum number of allowed xref `Prev` pointers before we abort.
const MAX_XREF_CHAIN_DEPTH: usize = 256;

fn populate_xref_impl_inner<'a>(
    data: &'a [u8],
    pos: usize,
    xref_map: &mut XrefMap,
    visited: &mut BTreeSet<usize>,
) -> Option<&'a [u8]> {
    if !visited.insert(pos) {
        warn!("circular xref PREV chain detected at offset {}", pos);

        return None;
    }

    if visited.len() > MAX_XREF_CHAIN_DEPTH {
        warn!(
            "xref PREV chain exceeds maximum depth of {}",
            MAX_XREF_CHAIN_DEPTH
        );

        return None;
    }

    let mut reader = Reader::new(data);
    reader.jump(pos);
    // In case the position points to before the object number of a xref stream.
    reader.skip_white_spaces_and_comments();

    let mut r2 = reader.clone();
    if reader
        .clone()
        .read_without_context::<ObjectIdentifier>()
        .is_some()
    {
        populate_from_xref_stream(data, &mut r2, xref_map, visited)
    } else {
        populate_from_xref_table(data, &mut r2, xref_map, visited)
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

/// Populate the xref table, and return the trailer dict.
fn populate_from_xref_table<'a>(
    data: &'a [u8],
    reader: &mut Reader<'a>,
    insert_map: &mut XrefMap,
    visited: &mut BTreeSet<usize>,
) -> Option<&'a [u8]> {
    let trailer = {
        let mut reader = reader.clone();
        read_xref_table_trailer(&mut reader, &ReaderContext::dummy())?
    };

    reader.skip_white_spaces();
    reader.forward_tag(b"xref")?;
    reader.skip_white_spaces();

    let mut max_obj = 0;

    if let Some(prev) = trailer.get::<i32>(PREV) {
        // First insert the entries from any previous xref tables.
        populate_xref_impl_inner(data, prev as usize, insert_map, visited)?;
    }

    // In hybrid files, entries in `XRefStm` should have higher priority, therefore we insert them
    // after looking at `PREV`.
    if let Some(xref_stm) = trailer.get::<i32>(XREF_STM) {
        populate_xref_impl_inner(data, xref_stm as usize, insert_map, visited)?;
    }

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

    Some(trailer.data())
}

fn populate_from_xref_stream<'a>(
    data: &'a [u8],
    reader: &mut Reader<'a>,
    insert_map: &mut XrefMap,
    visited: &mut BTreeSet<usize>,
) -> Option<&'a [u8]> {
    let stream = reader
        .read_with_context::<IndirectObject<Stream<'_>>>(&ReaderContext::dummy())?
        .get();

    if let Some(prev) = stream.dict().get::<i32>(PREV) {
        // First insert the entries from any previous xref tables.
        let _ = populate_xref_impl_inner(data, prev as usize, insert_map, visited)?;
    }

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
        let iter = arr.iter::<(u32, u32)>();

        for (start, num_elements) in iter {
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

    Some(stream.dict().data())
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
    for i in 0..num_elements {
        let f_type = if f1_len == 0 {
            1
        } else {
            // We assume a length of 1.
            xref_reader.read_bytes(1)?[0]
        };

        let obj_number = start + i;

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
    /// Each object's number and offset in `data`.
    offsets: Vec<(u32, usize)>,
    /// The index of the last object of each number, built the first time an entry's
    /// index names another object.
    by_number: OnceLock<FxHashMap<u32, usize>>,
}

impl ObjectStream {
    pub(crate) fn new(inner: &Stream<'_>) -> Option<Self> {
        let num_objects = inner.dict().get::<usize>(N)?;
        let first_offset = inner.dict().get::<usize>(FIRST)?;
        let data = inner.decoded().ok()?.into_owned();

        let mut r = Reader::new(&data);

        let mut offsets = vec![];

        for _ in 0..num_objects {
            r.skip_white_spaces_and_comments();
            // Skip object number
            let obj_num = r.read_without_context::<u32>()?;
            r.skip_white_spaces_and_comments();
            let relative_offset = r.read_without_context::<usize>()?;
            offsets.push((obj_num, first_offset.checked_add(relative_offset)?));
        }
        #[cfg(test)]
        OBJECT_STREAM_ENTRIES.set(OBJECT_STREAM_ENTRIES.get() + offsets.len());

        Some(Self {
            data,
            offsets,
            by_number: OnceLock::new(),
        })
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

        let mut xref_map = FxHashMap::default();
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

    /// The work opening `file` takes, after checking it opens to the 100 × 100 page.
    fn work(file: &[u8]) -> usize {
        OPEN_BYTES.set(0);
        let pdf = crate::Pdf::new(file.to_vec()).unwrap();
        let read = OPEN_BYTES.get();
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
        let families: [Family; 19] = [
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
                    work <= 12 * file.len(),
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
        let mut entries = FxHashMap::default();
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

    // An entry whose object holds the header another entry leads to, in a string, is
    // kept when the table is rebuilt for it: its object is read to its end.
    #[test]
    fn entries_into_a_string_of_another_object_are_kept() {
        for space in [false, true] {
            let pdf = crate::Pdf::new(nested(30, space, false)).unwrap();
            for number in 4..34 {
                let id = ObjectIdentifier::new(number, 0);
                let string = pdf.xref().get::<object::String<'_>>(id);
                assert!(string.is_some(), "object {number}, space {space}");
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

    /// The bytes of object headers read to open `file` and find its pages' sizes,
    /// once it is checked that each of the `pages` pages is 600 × 700 pt.
    fn header_bytes(file: Vec<u8>, pages: usize) -> usize {
        object::HEADER_BYTES.set(0);
        let pdf = crate::Pdf::new(file).unwrap();
        assert_eq!(pdf.pages().len(), pages);
        for page in pdf.pages().iter() {
            assert_eq!(page.render_dimensions(), (600.0, 700.0));
        }
        object::HEADER_BYTES.get()
    }

    // The header of an object that entries of an encrypted file's table lead to,
    // which names the key its bytes decrypt with, is read once for them all, not once
    // for each: reading the file is linear in its size, however long the header.
    #[test]
    fn a_header_entries_lead_to_is_read_once_for_them_all() {
        for (padded, encrypted) in [(true, true), (true, false), (false, true)] {
            let [small, large] = [256, 512]
                .map(|n| header_bytes(aliased_padded(n, padded, encrypted), n as usize + 1));
            assert!(
                large <= small * 25 / 10,
                "padded: {padded}, encrypted: {encrypted}: header bytes read for 256 \
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
        body::BODIES.with_borrow_mut(FxHashMap::clear);
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

    /// The bytes of headers read while every item of a page's /Annots, `n`
    /// references to string 4, whose header is padded with `64 * n` spaces where
    /// `padded`, is read as an annotation, as many times as it is listed.
    fn annotation_header_bytes(n: usize, padded: bool) -> usize {
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
        object::HEADER_BYTES.set(0);
        let annots = page.raw().get::<Array<'_>>(b"Annots".as_slice()).unwrap();
        for _ in 0..2 {
            assert_eq!(annots.iter_checked::<Dict<'_>>().count(), 0);
            assert_eq!(annots.iter::<Dict<'_>>().count(), 0);
        }
        object::HEADER_BYTES.get()
    }

    // The header of an object that many references lead to, read as another kind
    // than it is, is read once, however long: reading the references takes time
    // linear in the file.
    #[test]
    fn a_header_read_for_another_kind_of_object_is_read_once() {
        for padded in [true, false] {
            let [small, large] = [128, 256].map(|n| annotation_header_bytes(n, padded));
            assert!(
                large <= small * 25 / 10,
                "padded: {padded}: header bytes {small} for 128 references, {large} for 256"
            );
        }
        assert!(annotation_header_bytes(256, true) <= 512 * 256 + 16);
    }

    /// How often bodies were read, or streams' data found, while `run` ran, the most
    /// for one place.
    fn bodies_read(run: impl FnOnce()) -> usize {
        body::BODIES.with_borrow_mut(FxHashMap::clear);
        body::DATA_FOUND.with_borrow_mut(FxHashMap::clear);
        run();
        let most = |map: &FxHashMap<Place, usize>| map.values().copied().max().unwrap_or(0);
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
