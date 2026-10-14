//! Reading and querying the xref table of a PDF file.

use crate::crypto::reader::DecryptedReader;
use crate::crypto::{DecryptionError, DecryptionTarget, Decryptor, get};
use crate::data::Data;
use crate::metadata::Metadata;
use crate::object::Name;
use crate::object::ObjectIdentifier;
use crate::object::Stream;
use crate::object::dict::keys::{
    AUTHOR, CREATION_DATE, CREATOR, ENCRYPT, FIRST, ID, INDEX, INFO, KEYWORDS, MOD_DATE, N,
    OCPROPERTIES, PAGES, PREV, PRODUCER, ROOT, SIZE, SUBJECT, TITLE, TYPE, VERSION, W, XREF_STM,
};
use crate::object::dict::probe_dict;
use crate::object::indirect::IndirectObject;
use crate::object::{Array, MaybeRef};
use crate::object::{DateTime, Dict};
use crate::object::{Object, ObjectLike};
use crate::pdf::PdfVersion;
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt};
use crate::sync::{Arc, FxHashMap};
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

pub(crate) const XREF_ENTRY_LEN: usize = 20;

/// Identifies a loaded document among every document loaded in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
        return fallback(data, password, &[]).ok_or(XRefError::Unknown);
    };

    match check(&mut xref_map, data.as_ref()) {
        Ok(()) => match XRef::new(
            data.clone(),
            xref_map,
            XRefInput::TrailerDictData(trailer),
            password,
        ) {
            Err(XRefError::Unknown) => fallback(data, password, &[]).ok_or(XRefError::Unknown),
            xref => xref,
        },
        Err(listed) => fallback(data, password, &listed).ok_or(XRefError::Unknown),
    }
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
    /// The bytes that opening files has read on this thread to check their tables
    /// and to scan the files whose tables are rebuilt: what the read objects span,
    /// and the whole span a read was allowed where it failed. The tests bound it to
    /// show that opening a file reads each part of it a bounded number of times.
    pub(crate) static OPEN_BYTES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
fn opened(bytes: usize) {
    OPEN_BYTES.set(OPEN_BYTES.get() + bytes);
}

// Check that each entry leads to its object before any object is read: a table with
// an entry that does not is rebuilt, so the table never changes once read, and every
// value read from the document is read from the one table. Returns the entries that
// lead to an object, for the rebuilt table, if one does not lead to its own.
//
// An entry leads to its object where an object whose header names it is at its offset
// and ends before the next offset any entry names: objects of a table do not overlap.
// So each object is read once however many entries name it, and no further than the
// next, which reads each byte of the file once. MuPDF, too, rebuilds a table with an
// entry past the end of the file or in an object stream the table does not list as an
// object (`check_xref_entry_offsets` in MuPDF's `pdf-xref.c`); an entry at offset 0 is
// free.
fn check(xref_map: &mut XrefMap, data: &[u8]) -> Result<(), Vec<Listed>> {
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
    let mut listed = vec![];
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
                    listed.push(Listed {
                        id: *id,
                        offset,
                        header,
                    });
                }
                None => sound = false,
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
        Err(listed)
    }
}

/// The identifier in the header of the object `data` starts with, if the object ends
/// in `data`.
fn object_in(data: &[u8]) -> Option<ObjectIdentifier> {
    let mut r = Reader::new(data);
    let header = r.clone().read_without_context::<ObjectIdentifier>();
    let object = r.skip::<IndirectObject<Object<'_>>>(false);
    #[cfg(test)]
    opened(object.map_or(data.len(), <[u8]>::len));
    object.and(header)
}

// Try to manually parse the PDF to build an xref table and trailer dictionary.
//
// The objects the scan finds are the table's, the later of two that a damaged file
// numbers alike, as MuPDF rebuilds a table; an entry of the old table, `listed`, that
// leads to an object the scan does not find under the entry's number is kept. Such an
// object is the entry's, not the one its header names: where the scan finds it under
// its header's number, the scan takes the definition before it.
fn fallback(data: PdfData, password: &[u8], listed: &[Listed]) -> Option<XRef> {
    warn!("xref table was invalid, trying to manually build xref table");
    let (xref_map, xref_input) = fallback_xref_map(&data, password, listed);

    if let Some(xref_input) = xref_input {
        warn!("rebuild xref table with {} entries", xref_map.len());

        XRef::new(data.clone(), xref_map, xref_input, password).ok()
    } else {
        warn!("couldn't find trailer dictionary, failed to rebuild xref table");

        None
    }
}

fn fallback_xref_map<'a>(
    data: &'a PdfData,
    password: &[u8],
    listed: &[Listed],
) -> (XrefMap, Option<XRefInput<'a>>) {
    fallback_xref_map_inner(data, ReaderContext::dummy(), true, password, listed)
}

fn fallback_xref_map_inner<'a>(
    data: &'a PdfData,
    mut dummy_ctx: ReaderContext<'a>,
    recurse: bool,
    password: &[u8],
    listed: &[Listed],
) -> (XrefMap, Option<XRefInput<'a>>) {
    let mut xref_map = FxHashMap::default();
    // Each object found, in the order of the file.
    let mut found = vec![];
    let mut trailer_dicts = vec![];
    let mut root_ref = None;

    let mut r = Reader::new(data.as_ref());

    let mut last_obj_num = None;
    // The end of the part of the file an object was read past a header in.
    let mut read_past = 0;

    loop {
        let cur_pos = r.offset();

        let mut old_r = r.clone();

        // First try to check if we have an object identifier.
        if r.peek_byte().is_some_and(|b: u8| b.is_ascii_digit()) {
            if let Some(obj_id) = scan_header(&mut r) {
                // Check that the object following it is actually valid before inserting
                // it. It is read up to the next header, and past it only once for each
                // part of the file: a damaged object may run into the next, but the
                // headers in an object read past them, or after one that never ends,
                // are each read up to the next, not again to the end of the outer one.
                // The headers met in turn are each searched for from the last, so the
                // searches, like the reads, take time linear in the file.
                let data = data.as_ref();
                let end = next_header(data, r.offset());
                let mut valid = object_until(data, r.offset(), end);
                if valid.is_none() && end < data.len() && cur_pos >= read_past {
                    valid = object_until(data, r.offset(), data.len());
                    read_past = valid.unwrap_or(data.len());
                }
                if valid.is_some() {
                    xref_map.insert(obj_id, EntryType::Normal(cur_pos));
                    found.push((obj_id, cur_pos));
                    last_obj_num = Some(obj_id);
                    dummy_ctx.set_obj_number(obj_id);
                }
            } else {
                // There must be a white space before the next object number.
                r.forward_while(|b| !is_white_space_character(b));
            }
        } else {
            // Then, try to check whether we have a dictionary, in particular a trailer
            // dictionary.
            let mut probe_reader = r.clone();
            if r.peek_bytes(2).is_some_and(|b| b == b"<<")
                && let Some(probe) =
                    { probe_dict(&mut probe_reader, &dummy_ctx, Some(b"<<"), b">>") }
            {
                r = probe_reader;
                if probe.has_root || probe.has_type {
                    let mut dict_reader = Reader::new(probe.data);
                    if let Some(dict) = dict_reader.read_with_context::<Dict<'_>>(&dummy_ctx) {
                        if probe.has_root && dict.contains_key(ROOT) {
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

    keep_listed(&mut xref_map, &found, listed);

    // Try to choose the right trailer dict by doing basic validation.
    let mut trailer_dict = None;

    for dict in trailer_dicts {
        if let Some(root_id) = dict.get_raw::<Dict<'_>>(ROOT) {
            let check = |dict: &Dict<'_>| -> bool { dict.contains_key(PAGES) };

            match root_id {
                MaybeRef::Ref(r) => match xref_map.get(&r.into()) {
                    Some(EntryType::Normal(offset)) => {
                        let mut reader = Reader::new(&data.as_ref()[*offset..]);
                        let mut ctx = dummy_ctx.clone();
                        ctx.set_obj_number(r.into());

                        if let Some(obj) =
                            reader.read_with_context::<IndirectObject<Dict<'_>>>(&ctx)
                            && {
                                let obj = obj.get();
                                check(&obj)
                            }
                        {
                            trailer_dict = Some(dict);
                        }
                    }
                    Some(EntryType::ObjStream(obj_num, idx)) => {
                        let stream_id = ObjectIdentifier::new(*obj_num as i32, 0);
                        if let Some(EntryType::Normal(offset)) = xref_map.get(&stream_id) {
                            let mut reader = Reader::new(&data.as_ref()[*offset..]);
                            let mut ctx = dummy_ctx.clone();
                            ctx.set_obj_number(stream_id);

                            if let Some(stream) =
                                reader.read_with_context::<IndirectObject<Stream<'_>>>(&ctx)
                                && {
                                    let stream = stream.get();
                                    if let Some(object_stream) = ObjectStream::new(&stream)
                                        && let Some(obj) =
                                            object_stream.get::<Dict<'_>>(*idx, &dummy_ctx)
                                    {
                                        check(&obj)
                                    } else {
                                        false
                                    }
                                }
                            {
                                trailer_dict = Some(dict);
                            }
                        }
                    }
                    _ => {}
                },
                MaybeRef::NotRef(d) => {
                    if check(&d) {
                        trailer_dict = Some(dict);
                    }
                }
            }
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
            XRefInput::TrailerDictData(trailer_dict.as_ref().map(|d| d.data()).unwrap()),
            password,
        ) {
            let ctx = ReaderContext::new(&xref, false);
            let (patched_map, _) = fallback_xref_map_inner(data, ctx, false, password, listed);
            xref_map = patched_map;
        }
    }

    if let Some(trailer_dict_data) = trailer_dict.map(|d| d.data()) {
        (
            xref_map,
            Some(XRefInput::TrailerDictData(trailer_dict_data)),
        )
    } else if let Some(root_ref) = root_ref {
        (xref_map, Some(XRefInput::RootRef(root_ref)))
    } else {
        (xref_map, None)
    }
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

/// Where the object at `from` in `data[..to]` ends, if it ends there.
fn object_until(data: &[u8], from: usize, to: usize) -> Option<usize> {
    let mut object = Reader::new(&data[..to]);
    object.jump(from);
    object.skip_white_spaces_and_comments();
    let end = object.skip::<Object<'_>>(false).map(|_| object.offset());
    #[cfg(test)]
    opened(end.unwrap_or(to) - from);
    end
}

/// Where the first header at or after `from` starts, or the end of `data`: a header
/// begins a number that follows no digit, outside a comment.
fn next_header(data: &[u8], from: usize) -> usize {
    let mut at = from;
    while at < data.len() {
        if data[at] == b'%' {
            at += data[at..]
                .iter()
                .position(|b| is_eol_character(*b))
                .unwrap_or(data.len() - at);
        } else if data[at].is_ascii_digit()
            && !data[at - 1].is_ascii_digit()
            && scan_header(&mut Reader::new(&data[at..])).is_some()
        {
            return at;
        } else {
            at += 1;
        }
    }
    data.len()
}

/// Adds to the rebuilt table `xref_map` the entries of the old table, `listed`, whose
/// number the scan did not find. The object such an entry leads to is the entry's:
/// where its header names another number, the scan takes for that number the last
/// definition no such entry leads to, unless the old table, too, lists that number
/// at the entry's offset.
fn keep_listed(xref_map: &mut XrefMap, found: &[(ObjectIdentifier, usize)], listed: &[Listed]) {
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
    for entry in kept {
        xref_map.insert(entry.id, EntryType::Normal(entry.offset));
    }
}

const DUMMY_XREF: XRef = XRef(Inner::Dummy);

#[cfg(test)]
std::thread_local! {
    /// The entries of object streams read on this thread, which the tests bound to
    /// show that each object stream is read once.
    pub(crate) static OBJECT_STREAM_ENTRIES: core::cell::Cell<usize> =
        const { core::cell::Cell::new(0) };
    /// While `Some`, how often each object has been resolved on this thread, which
    /// the tests bound to show that the page tree resolves each object once.
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
                elements.sort_by(|e1, e2| e1.1.cmp(&e2.1));

                let mut iter = elements.into_iter();

                iter::from_fn(move || {
                    for next in iter.by_ref() {
                        if let Some(obj) = self.get_with(next.0, &ReaderContext::new(self, false)) {
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

    /// Whether the table lists object `id`; a reference to an object it does not list
    /// is `null`.
    pub fn has_entry(&self, id: ObjectIdentifier) -> bool {
        match &self.0 {
            Inner::Dummy => false,
            Inner::Some(r) => r.map.contains_key(&id),
        }
    }

    /// Return the object with the given identifier.
    #[allow(private_bounds)]
    pub fn get<'a, T>(&'a self, id: ObjectIdentifier) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        let ctx = ReaderContext::new(self, false);
        self.get_with(id, &ctx)
    }

    /// Return the object with the given identifier.
    #[allow(private_bounds)]
    pub(crate) fn get_with<'a, T>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        #[cfg(test)]
        RESOLVED.with_borrow_mut(|resolved| {
            if let Some(resolved) = resolved {
                *resolved.entry(id).or_default() += 1;
            }
        });

        let Inner::Some(repr) = &self.0 else {
            return None;
        };

        // References to undefined objects are treated as null.
        let entry = *repr.map.get(&id)?;

        if ctx.parent_chain_contains(&id) {
            warn!("cycle detected in indirect object: {id:?}");

            return None;
        }

        // The object is `id`, the entry it was reached through: what is read from it
        // is decrypted, cached and checked for cycles as `id`, whatever its header
        // says. This is the only place that names the object a context reads.
        let mut ctx = ctx.clone();
        ctx.set_obj_number(id);
        ctx.parent_chain_push(id);
        ctx.set_in_content_stream(false);

        match entry {
            // An object is at the offset, as `check` or the scan that rebuilt the
            // table found it; one that is not of the type the caller expects is no
            // such object.
            EntryType::Normal(offset) => {
                ctx.set_in_object_stream(false);
                let mut r = Reader::new(repr.data.get().as_ref());
                r.jump(offset);

                r.read_with_context::<IndirectObject<T>>(&ctx)
                    .map(IndirectObject::get)
            }
            EntryType::ObjStream(obj_stram_gen_num, index) => {
                // Generation number is implicitly 0.
                let obj_stream_id = ObjectIdentifier::new(obj_stram_gen_num as i32, 0);

                if obj_stream_id == id {
                    warn!("cycle detected in object stream");

                    return None;
                }

                repr.data.get_with(obj_stream_id, &ctx)?.get(index, &ctx)
            }
        }
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
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
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

        Some(Self { data, offsets })
    }

    fn get<'a, T>(&'a self, index: u32, ctx: &ReaderContext<'a>) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        let offset = self.offsets.get(index as usize)?.1;
        let mut r = Reader::new(&self.data);
        r.jump(offset);
        r.skip_white_spaces_and_comments();

        let mut ctx = ctx.clone();
        ctx.set_in_object_stream(true);
        r.read_with_context::<T>(&ctx)
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
        let body = format!("4 0 obj\n{}\nendobj\n", unit.repeat(n));
        file_with(body.as_bytes(), &[0], true)
    }

    /// The bytes opening `file` reads, after checking it opens to the 100 × 100 page.
    fn bytes_read(file: &[u8]) -> usize {
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
    // strings and comments the objects hold.
    #[test]
    fn opening_a_file_reads_it_in_linear_time() {
        type Family = (&'static str, fn(usize) -> Vec<u8>);
        let families: [Family; 9] = [
            ("aliases", |n| aliases(n, false)),
            ("nested", |n| nested(n, false, false)),
            ("nested after space", |n| nested(n, true, false)),
            ("aliases, rebuilt", |n| aliases(n, true)),
            ("nested, rebuilt", |n| nested(n, false, true)),
            ("nested after space, rebuilt", |n| nested(n, true, true)),
            ("unterminated strings", |n| repeated("( 5 0 obj ", n)),
            ("comments", |n| repeated("1 %x ", n)),
            ("headers split by comments", |n| repeated("9 %x\n0 obj ", n)),
        ];
        for (family, file) in families {
            let [small, large] = [1000, 2000].map(|n| {
                let file = file(n);
                let read = bytes_read(&file);
                assert!(
                    read <= 6 * file.len(),
                    "{family}: {read} bytes read to open {} bytes",
                    file.len()
                );
                read
            });
            assert!(
                large <= small * 22 / 10,
                "{family}: {small} bytes read for 1000, {large} for 2000"
            );
        }
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
}
