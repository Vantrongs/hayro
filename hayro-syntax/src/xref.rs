//! Reading and querying the xref table of a PDF file.

use crate::crypto::{DecryptionError, DecryptionTarget, Decryptor, get};
use crate::data::Data;
use crate::hash::Map;
use crate::lost::Lost;
use crate::metadata::Metadata;
use crate::object::Name;
use crate::object::ObjectIdentifier;
use crate::object::Stream;
use crate::object::Written;
use crate::object::dict::keys::{
    AUTHOR, CREATION_DATE, CREATOR, ENCRYPT, FIRST, ID, INDEX, INFO, KEYWORDS, MOD_DATE, N,
    OCPROPERTIES, PAGES, PREV, PRODUCER, ROOT, SIZE, SUBJECT, TITLE, TYPE, VERSION, W, XREF_STM,
};
use crate::object::indirect::IndirectObject;
use crate::object::stream::NextEndstream;
use crate::object::{Array, MaybeRef};
use crate::object::{DateTime, Dict};
use crate::object::{Object, ObjectLike};
use crate::pdf::PdfVersion;
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, skip_white_spaces_and_comments_in};
use crate::sync::Arc;
use crate::trivia::is_regular_character;
use crate::trivia::is_white_space_character;
use crate::util::findr_needle;
use crate::{PdfData, object};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::max;
use core::ops::Deref;
use token_ends::LazyEnds;

mod token_ends;

mod containers;

pub(crate) const XREF_ENTRY_LEN: usize = 20;

#[derive(Debug, Copy, Clone)]
pub(crate) enum XRefError {
    Unknown,
    Encryption(DecryptionError),
}

pub(crate) fn root_xref(data: PdfData, password: &[u8]) -> Result<XRef, XRefError> {
    let mut xref_map = Map::default();
    let xref_pos = find_last_xref_pos(data.as_ref()).ok_or(XRefError::Unknown)?;
    let trailer =
        populate_xref_impl(data.as_ref(), xref_pos, &mut xref_map).ok_or(XRefError::Unknown)?;

    xref_map.retain(|_, entry| *entry != EntryType::Normal(0));

    XRef::new(
        data.clone(),
        xref_map,
        XRefInput::TrailerDictData(trailer),
        false,
        password,
        Map::default(),
    )
}

pub(crate) fn fallback(data: PdfData, password: &[u8]) -> Option<XRef> {
    let mut broken = Map::default();
    let (xref_map, xref_input) = scan_table(&data, password, &mut broken, None);

    if let Some(xref_input) = xref_input {
        XRef::new(data.clone(), xref_map, xref_input, true, password, broken).ok()
    } else {
        None
    }
}

#[cfg(test)]
fn fallback_xref_map<'a>(data: &'a PdfData, password: &[u8]) -> (XrefMap, Option<XRefInput<'a>>) {
    scan_table(data, password, &mut Map::default(), None)
}
type Broken = Map<ObjectIdentifier, usize>;
fn scan_table<'a>(
    data: &'a PdfData,
    password: &[u8],
    broken: &mut Broken,
    listed: Option<&XrefMap>,
) -> (XrefMap, Option<XRefInput<'a>>) {
    let ends = LazyEnds::new(data.as_ref());
    fallback_xref_map_inner(
        data,
        &ends,
        ReaderContext::dummy(),
        true,
        password,
        broken,
        listed,
    )
}

fn fallback_xref_map_inner<'a>(
    data: &'a PdfData,
    ends: &LazyEnds<'_>,
    mut dummy_ctx: ReaderContext<'a>,
    recurse: bool,
    password: &[u8],
    broken: &mut Broken,
    listed: Option<&XrefMap>,
) -> (XrefMap, Option<XRefInput<'a>>) {
    let mut xref_map = Map::default();
    let mut claims: Vec<_> = listed
        .into_iter()
        .flatten()
        .filter_map(|(&id, entry)| match entry {
            EntryType::Normal(at) => Some((*at, id)),
            _ => None,
        })
        .collect();
    claims.sort_unstable_by_key(|claim| claim.0);
    let mut claims = claims.chunk_by(|a, b| a.0 == b.0).peekable();
    let mut aliases = Vec::new();
    let mut found = Vec::new();
    let mut preferred = Vec::new();
    let mut trailer_dicts = vec![];
    let mut root_ref = None;

    let mut r = Reader::new(data.as_ref());

    let mut containers = containers::Containers::default();
    let mut last_obj_num = None;
    let mut next_endstream = NextEndstream::default();

    loop {
        let cur_pos = r.offset();
        containers.evict_before(cur_pos);

        if r.peek_byte().is_some_and(|b: u8| b.is_ascii_digit()) {
            let mut header = r.clone();
            if let Some(obj_id) = ObjectIdentifier::read_in(&mut header, ends) {
                r = header;
                let mut cloned = r.clone();
                skip_white_spaces_and_comments_in(&mut cloned, ends);
                let body_start = cloned.offset();
                if containers.skip_object(ends, &mut cloned).is_some() {
                    while claims.next_if(|group| group[0].0 < cur_pos).is_some() {}
                    let first_alias = aliases.len();
                    let mut named = false;
                    while let Some(group) = claims.next_if(|group| group[0].0 < body_start) {
                        let at = group[0].0;
                        let end = claims.peek().map_or(data.as_ref().len(), |next| {
                            next[0].0.min(data.as_ref().len())
                        });
                        let valid = if at == cur_pos {
                            end >= r.offset()
                        } else {
                            let mut slot = Reader::new(&data.as_ref()[at..end]);
                            ObjectIdentifier::read_in(&mut slot, &crate::reader::Unknown).is_some()
                        };
                        if valid {
                            for &(_, id) in group {
                                named |= id == obj_id;
                                if id != obj_id {
                                    aliases.push((id, cur_pos));
                                }
                            }
                        } else if at == cur_pos {
                            broken.extend(group.iter().map(|&(_, id)| (id, at)));
                        }
                    }
                    if listed.is_some() {
                        found.push((obj_id, cur_pos, !named && aliases.len() != first_alias));
                    }
                    xref_map.insert(obj_id, EntryType::Normal(cur_pos));
                    last_obj_num = Some(obj_id);
                    dummy_ctx.set_obj_number(obj_id);
                } else {
                    broken.insert(obj_id, cur_pos);
                }
            } else {
                r.forward_while(|b| !is_white_space_character(b));
            }
        } else {
            if r.peek_bytes(2).is_some_and(|b| b == b"<<")
                && let Some(probe) = containers.resolve(ends, r.clone())
            {
                r.jump(probe.end);
                let mut payload = r.clone();
                let has_stream =
                    object::stream::forward_stream_opener(&mut payload, ends).is_some();
                if probe.has_root || probe.has_type || has_stream {
                    let mut dict_reader = Reader::new(&data.as_ref()[cur_pos..probe.end]);
                    if let Some(dict) = dict_reader.read_with_context::<Dict<'_>>(&dummy_ctx) {
                        if dict
                            .get::<Name<'_>>(TYPE)
                            .is_some_and(|n| n.as_str() == "Catalog")
                        {
                            root_ref = last_obj_num;
                        }

                        if dict.get::<Name<'_>>(TYPE).as_deref() == Some(b"ObjStm")
                            && has_usable_object_stream_header(&dict)
                            && let Some(last_obj_num) = last_obj_num
                            && let Some((stream, _)) = Stream::read_after_dict(
                                &mut r.clone(),
                                dict.clone(),
                                ends,
                                &mut next_endstream,
                            )
                            && let Ok(data) = stream.decoded_data()
                            && let Some(offsets) = ObjectStream::read_offsets(&stream, &data)
                        {
                            for (idx, (obj_num, offset)) in offsets.into_iter().enumerate() {
                                if offset.is_none() {
                                    continue;
                                }

                                let id = ObjectIdentifier::new(obj_num, 0);
                                let entry =
                                    EntryType::ObjStream(last_obj_num.obj_number, idx as u32);
                                if let Some(&location) = xref_map.get(&last_obj_num) && listed.is_some_and(|map| {
                                    matches!(map.get(&id), Some(EntryType::ObjStream(num, _)) if *num == last_obj_num.obj_number)
                                        && map.get(&last_obj_num) == Some(&location)
                                }) {
                                    preferred.push((
                                        id,
                                        entry,
                                        last_obj_num,
                                        location,
                                    ));
                                }
                                if xref_map
                                    .get(&id)
                                    .is_none_or(|e| !matches!(e, &EntryType::Normal(_)))
                                {
                                    xref_map.insert(id, entry);
                                }
                            }
                        }

                        // An exact declared length owns its payload, including bytes that look like headers.
                        if has_stream
                            && let Some(length) = dict.get::<usize>(object::dict::keys::LENGTH)
                            && payload.skip_bytes(length).is_some()
                        {
                            next_endstream.skip_white_spaces(&mut payload);
                            if payload.forward_tag(b"endstream").is_some() {
                                r = payload;
                            }
                        }
                        if probe.has_root && dict.contains_key(ROOT) {
                            trailer_dicts.push(dict);
                        }
                    }
                }
            } else {
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

    drop(containers);
    for (id, entry, carrier, location) in preferred {
        if xref_map.get(&carrier) == Some(&location) {
            xref_map.insert(id, entry);
        }
    }
    aliases.retain(|(id, _)| !xref_map.contains_key(id));
    if !aliases.is_empty() {
        let taken: crate::hash::Set<_> = aliases.iter().map(|&(_, at)| at).collect();
        let mut moved = crate::hash::Set::default();
        for &(id, at, exclusive) in &found {
            if exclusive && taken.contains(&at) && xref_map.get(&id) == Some(&EntryType::Normal(at))
            {
                moved.insert(id);
                xref_map.remove(&id);
            }
        }
        for (id, at, exclusive) in found {
            if moved.contains(&id) && !(exclusive && taken.contains(&at)) {
                xref_map.insert(id, EntryType::Normal(at));
            }
        }
        xref_map.extend(
            aliases
                .into_iter()
                .map(|(id, at)| (id, EntryType::Normal(at))),
        );
    }
    let trailer_dict = find_rebuild_trailer(data, &dummy_ctx, &xref_map, trailer_dicts);

    let has_encryption = trailer_dict
        .as_ref()
        .is_some_and(|t| t.contains_key(ENCRYPT));

    if has_encryption && recurse {
        // Redo with a decrypting context to find objects in encrypted object streams.
        if let Ok(xref) = XRef::new(
            data.clone(),
            xref_map.clone(),
            XRefInput::TrailerDictData(trailer_dict.as_ref().map(|d| d.data()).unwrap()),
            true,
            password,
            broken.clone(),
        ) {
            let ctx = ReaderContext::new(&xref, false);
            let (patched_map, _) =
                fallback_xref_map_inner(data, ends, ctx, false, password, broken, listed);
            xref_map = patched_map;
        }
    }

    broken.retain(|id, _| !xref_map.contains_key(id));
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

fn find_rebuild_trailer<'a>(
    data: &'a PdfData,
    ctx: &ReaderContext<'a>,
    xref_map: &XrefMap,
    mut trailers: Vec<Dict<'a>>,
) -> Option<Dict<'a>> {
    let mut checked = Map::default();
    let mut groups = None;
    let read_stream = |num| {
        let EntryType::Normal(offset) = *xref_map.get(&ObjectIdentifier::new(num, 0))? else {
            return None;
        };
        let dict = Reader::new(&data.as_ref()[offset..])
            .read_with_context::<IndirectObject<Dict<'_>>>(ctx)?;
        if !has_usable_object_stream_header(&dict.get()) {
            return None;
        }
        let stream = Reader::new(&data.as_ref()[offset..])
            .read_with_context::<IndirectObject<Stream<'_>>>(ctx)?;
        ObjectStream::new(&stream.get())
    };

    while let Some(trailer) = trailers.pop() {
        let id = match trailer.get_raw::<Dict<'_>>(ROOT) {
            Some(MaybeRef::NotRef(dict)) if dict.contains_key(PAGES) => return Some(trailer),
            Some(MaybeRef::Ref(reference)) => ObjectIdentifier::from(reference),
            _ => continue,
        };
        if let Some(&valid) = checked.get(&id) {
            if valid {
                return Some(trailer);
            }
            continue;
        }

        match xref_map.get(&id).copied() {
            Some(EntryType::Normal(offset)) => {
                if Reader::new(&data.as_ref()[offset..])
                    .read_with_context::<IndirectObject<Dict<'_>>>(ctx)
                    .is_some_and(|obj| obj.get().contains_key(PAGES))
                {
                    return Some(trailer);
                }
                checked.insert(id, false);
            }
            Some(EntryType::ObjStream(num, idx)) => {
                let stream = read_stream(num);
                let valid = |idx| {
                    stream
                        .as_ref()
                        .and_then(|stream| stream.get::<Dict<'_>>(idx, ctx))
                        .is_some_and(|dict| dict.contains_key(PAGES))
                };
                if valid(idx) {
                    return Some(trailer);
                }
                checked.insert(id, false);

                // Group remaining roots once so interleaved trailers can share a decode.
                let groups = groups.get_or_insert_with(|| {
                    let mut groups = Map::default();
                    for trailer in &trailers {
                        if let Some(reference) = trailer.get_ref(ROOT) {
                            let id = ObjectIdentifier::from(reference);
                            if let Some(&EntryType::ObjStream(num, idx)) = xref_map.get(&id) {
                                groups.entry(num).or_insert_with(Vec::new).push((id, idx));
                            }
                        }
                    }
                    groups
                });
                if let Some(members) = groups.remove(&num) {
                    for (id, idx) in members {
                        checked.entry(id).or_insert_with(|| valid(idx));
                    }
                }
            }
            None => continue,
        }
    }
    None
}

const DUMMY_XREF: XRef = XRef(Inner::Dummy);

/// The cross-reference table, checked or rebuilt on open and immutable thereafter.
#[derive(Debug, Clone)]
pub struct XRef(Inner);

/// Identifies source bytes by location, document and physical decryption key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentKey<'a> {
    location: (usize, usize),
    document: usize,
    decryption: Option<ObjectIdentifier>,
    lifetime: core::marker::PhantomData<&'a [u8]>,
}
impl<'a> ContentKey<'a> {
    pub(crate) fn new(bytes: &'a [u8], ctx: &ReaderContext<'a>) -> Self {
        let document = match &ctx.xref().0 {
            Inner::Some(repr) => Arc::as_ptr(&repr.data) as usize,
            Inner::Dummy => 0,
        };
        Self {
            location: (bytes.as_ptr() as usize, bytes.len()),
            document,
            decryption: ctx
                .obj_number()
                .filter(|_| ctx.xref().needs_decryption(ctx)),
            lifetime: core::marker::PhantomData,
        }
    }
}

impl XRef {
    fn new(
        data: PdfData,
        xref_map: XrefMap,
        input: XRefInput<'_>,
        repaired: bool,
        password: &[u8],
        broken: Broken,
    ) -> Result<Self, XRefError> {
        let original = MapRepr::new(xref_map, repaired, broken);
        let (map, trailer) = if !repaired && !original.valid(data.as_ref()) {
            original.rebuild(&data, password)
        } else {
            (original, None)
        };
        let input = trailer.unwrap_or(input);
        // The trailer can only be resolved once the xref exists.
        let trailer_data = TrailerData::dummy();

        let mut xref = Self(Inner::Some(Arc::new(SomeRepr {
            data: Arc::new(Data::new(data.clone())),
            map: Arc::new(map),
            decryptor: Arc::new(Decryptor::None),
            has_ocgs: false,
            metadata: Arc::new(Metadata::default()),
            trailer_data,
        })));

        // Read the trailer twice so the catalog can be in an encrypted object stream.

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
                let root = xref.get::<Dict<'_>>(root_ref).ok_or(XRefError::Unknown)?;

                let td = TrailerData {
                    root_ref,
                    version: None,
                };

                (
                    td,
                    root.get::<Dict<'_>>(OCPROPERTIES).is_some(),
                    Metadata::default(),
                )
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
            Inner::Some(r) => r.map.xref_map.len(),
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

    pub(crate) fn objects<'a, T: Readable<'a> + 'a>(&'a self) -> impl IntoIterator<Item = T> + 'a {
        self.ids_in_file_order()
            .filter_map(|id| self.get_with(id, &ReaderContext::new(self, false)))
    }

    pub(crate) fn ids_in_file_order(&self) -> impl Iterator<Item = ObjectIdentifier> {
        let Inner::Some(r) = &self.0 else {
            unreachable!()
        };
        let map = &r.map;
        let mut elements = map
            .xref_map
            .iter()
            .map(|(id, e)| {
                let offset = match e {
                    EntryType::Normal(o) => (*o, 0),
                    EntryType::ObjStream(id, index) => {
                        if let Some(EntryType::Normal(offset)) =
                            map.xref_map.get(&ObjectIdentifier::new(*id, 0))
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

        elements.extend(map.broken.iter().map(|(id, at)| (*id, (*at, 0))));
        elements.sort_unstable_by_key(|(id, location)| (*location, *id));
        elements.into_iter().map(|(id, _)| id)
    }

    pub(crate) fn storage(&self) -> Option<&Data> {
        match &self.0 {
            Inner::Some(r) => Some(&r.data),
            Inner::Dummy => None,
        }
    }

    /// Reads the value with each number in its source spelling.
    pub fn written(&self, id: ObjectIdentifier) -> Option<Written<'_>> {
        self.get_with(id, &ReaderContext::new(self, false))
    }

    /// Runs a computation and reports its first unreadable listed object.
    pub fn watch<T>(&self, f: impl FnOnce() -> T) -> (T, Lost) {
        match &self.0 {
            Inner::Some(r) => r.data.watched.watch(true, f),
            Inner::Dummy => (f(), None),
        }
    }
    /// Report the first unreadable object independently of the enclosing computation.
    pub fn apart<T>(&self, f: impl FnOnce() -> T) -> (T, Lost) {
        match &self.0 {
            Inner::Some(r) => r.data.watched.watch(false, f),
            Inner::Dummy => (f(), None),
        }
    }
    /// Replays the loss of a cached computation.
    pub fn note(&self, lost: Lost) {
        if let Inner::Some(r) = &self.0 {
            r.data.watched.note(lost);
        }
    }

    /// Location a listed reference leads to.
    pub fn content_key(&self, id: ObjectIdentifier) -> Option<ContentKey<'_>> {
        let Inner::Some(r) = &self.0 else { return None };
        let (entry, next) = r.map.location(id, self)?;
        let mut ctx = ReaderContext::new(self, false);
        let data = match entry {
            EntryType::Normal(at) => {
                let file = r.data.get().as_ref();
                let end = next.unwrap_or(file.len()).min(file.len());
                let Some(bytes) = file.get(at..end) else {
                    self.note(Some(id));
                    return None;
                };
                let physical = Reader::new(bytes).read_without_context::<ObjectIdentifier>();
                ctx.set_obj_number(physical.unwrap_or(id));
                bytes
            }
            EntryType::ObjStream(n, i) => {
                let carrier = ObjectIdentifier::new(n, 0);
                if carrier == id {
                    self.note(Some(id));
                    return None;
                }
                let (stream, lost) = self.apart(|| r.data.object_stream(carrier, &ctx));
                let Some((stream, at)) = stream.and_then(|s| {
                    let index = s.member_index(id.obj_number, i)?;
                    Some((s, s.offsets[index as usize].1?))
                }) else {
                    self.note(Some(id));
                    return None;
                };
                self.note(lost);
                ctx.set_in_object_stream(true);
                &stream.data[at..]
            }
        };
        Some(ContentKey::new(data, &ctx))
    }

    /// Whether the cross-reference map has been rebuilt.
    pub fn is_repaired(&self) -> bool {
        matches!(&self.0, Inner::Some(r) if r.map.repaired)
    }

    #[inline]
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

    pub(crate) fn decryptor(&self) -> &Decryptor {
        match &self.0 {
            Inner::Dummy => &Decryptor::None,
            Inner::Some(r) => &r.decryptor,
        }
    }

    #[inline]
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

    /// Return the object with the given identifier.
    #[allow(private_bounds)]
    pub fn get<'a, T>(&'a self, id: ObjectIdentifier) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        let ctx = ReaderContext::new(self, false);
        self.get_with(id, &ctx)
    }

    #[allow(private_bounds)]
    pub(crate) fn get_with<'a, T>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Option<T>
    where
        T: Readable<'a>,
    {
        let Inner::Some(repr) = &self.0 else {
            return None;
        };

        if ctx.parent_chain_contains(&id) {
            return None;
        }

        let (entry, next) = repr.map.location(id, self)?;
        let missing = || {
            self.note(Some(id));
            None
        };
        let mut ctx = ctx.clone();
        ctx.set_lookup_id(id);
        ctx.set_obj_number(id);
        ctx.set_in_content_stream(false);

        match entry {
            EntryType::Normal(offset) => {
                let file = repr.data.get().as_ref();
                let end = next.unwrap_or(file.len()).min(file.len());
                ctx.set_in_object_stream(false);
                let mut r = Reader::new_with(&file[..end], offset);
                if let Some(physical) = r.read_without_context::<ObjectIdentifier>() {
                    ctx.set_obj_number(physical);
                    ctx.parent_chain_push(id);
                    r.skip_white_spaces_and_comments();
                    ctx.mark_storage(r.tail()?);
                    ctx.set_cut(file.get(end).copied());
                    let (value, lost) = self.apart(|| {
                        read_bounded(&mut r, file.get(end).copied(), end, &ctx, id, true)
                    });
                    let lost = if lost == Some(physical) {
                        Some(id)
                    } else {
                        lost
                    };
                    self.note(lost);
                    return value;
                }
                missing()
            }
            EntryType::ObjStream(obj_stram_gen_num, index) => {
                let obj_stream_id = ObjectIdentifier::new(obj_stram_gen_num, 0);

                if obj_stream_id == id {
                    return missing();
                }

                if !matches!(
                    repr.map.xref_map.get(&obj_stream_id),
                    Some(EntryType::Normal(_))
                ) {
                    return missing();
                }

                ctx.parent_chain_push(id);

                let (object_stream, lost) =
                    self.apart(|| repr.data.object_stream(obj_stream_id, &ctx));
                let Some(object_stream) = object_stream else {
                    return missing();
                };

                if let Some(index) = object_stream.member_index(id.obj_number, index) {
                    self.note(lost);
                    return object_stream.member(index, &ctx, true);
                }

                missing()
            }
        }
    }
}

// One lookahead byte distinguishes a cut header from an invalid token in the slot.
fn cut_header(bytes: &[u8]) -> bool {
    if bytes.len() < 2 {
        return false;
    }
    let mut r = Reader::new(bytes);
    for generation in [false, true] {
        let Some(value) = r.read_without_context::<object::Number>() else {
            return matches!(r.tail(), Some(b"+" | b"-"));
        };
        if r.at_end() {
            return true;
        }
        let n = value.as_i64();
        if (generation && i32::try_from(n).is_err()) || (!generation && u32::try_from(n).is_err()) {
            return false;
        }
        r.skip_white_spaces_and_comments();
        if r.at_end() {
            return true;
        }
    }
    b"obj".starts_with(r.tail().unwrap_or_default())
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum XRefInput<'a> {
    TrailerDictData(&'a [u8]),
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

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum EntryType {
    Normal(usize),
    ObjStream(u32, u32),
}

type XrefMap = Map<ObjectIdentifier, EntryType>;

#[derive(Debug)]
struct MapRepr {
    xref_map: XrefMap,
    repaired: bool,
    offsets: Vec<usize>,
    broken: Broken,
}

impl MapRepr {
    fn valid(&self, file: &[u8]) -> bool {
        self.xref_map.iter().all(|(&id, &entry)| match entry {
            EntryType::Normal(at) => {
                let end = self.next_offset(at).unwrap_or(file.len()).min(file.len());
                file.get(at..end).is_some_and(|slot| {
                    let mut header = Reader::new(slot);
                    ObjectIdentifier::read_in(&mut header, &crate::reader::Unknown) == Some(id)
                })
            }
            EntryType::ObjStream(num, _) => matches!(
                self.xref_map.get(&ObjectIdentifier::new(num, 0)),
                Some(EntryType::Normal(_))
            ),
        })
    }

    fn rebuild<'a>(&self, data: &'a PdfData, password: &[u8]) -> (Self, Option<XRefInput<'a>>) {
        let mut broken = Broken::default();
        let (mut rebuilt, trailer) = scan_table(data, password, &mut broken, Some(&self.xref_map));
        let file = data.as_ref();
        let mut headers = Map::default();
        for (&id, &entry) in &self.xref_map {
            if rebuilt.contains_key(&id) {
                continue;
            }
            if let EntryType::ObjStream(_, _) = entry {
                rebuilt.entry(id).or_insert(entry);
            } else if let EntryType::Normal(at) = entry {
                let (physical, cut) = *headers.entry(at).or_insert_with(|| {
                    let end = self.next_offset(at).unwrap_or(file.len()).min(file.len());
                    let mut header = Reader::new_with(&file[..end], at);
                    (
                        ObjectIdentifier::read_in(&mut header, &crate::reader::Unknown),
                        file.get(at..=end).is_some_and(cut_header),
                    )
                });
                if let Some(physical) = physical {
                    if broken.contains_key(&physical) {
                        broken.insert(id, at);
                    } else {
                        rebuilt.entry(id).or_insert(entry);
                    }
                } else if cut {
                    broken.entry(id).or_insert(at);
                }
            }
        }
        (Self::new(rebuilt, true, broken), trailer)
    }

    fn location(&self, id: ObjectIdentifier, xref: &XRef) -> Option<(EntryType, Option<usize>)> {
        let entry = self.entry(id, xref)?;
        let end = match entry {
            EntryType::Normal(at) => self.next_offset(at),
            EntryType::ObjStream(_, _) => None,
        };
        Some((entry, end))
    }

    fn entry(&self, id: ObjectIdentifier, xref: &XRef) -> Option<EntryType> {
        let entry = self.xref_map.get(&id).copied();
        if entry.is_none() && self.broken.contains_key(&id) {
            xref.note(Some(id));
        }
        entry
    }

    fn new(xref_map: XrefMap, repaired: bool, broken: Broken) -> Self {
        let mut offsets: Vec<_> = xref_map
            .values()
            .filter_map(|e| {
                if let EntryType::Normal(at) = e {
                    Some(*at)
                } else {
                    None
                }
            })
            .collect();
        offsets.extend(broken.values().copied());
        offsets.sort_unstable();
        offsets.dedup();
        Self {
            xref_map,
            repaired,
            offsets,
            broken,
        }
    }
    fn next_offset(&self, at: usize) -> Option<usize> {
        self.offsets
            .get(self.offsets.partition_point(|o| *o <= at))
            .copied()
    }
}

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
    data: Arc<Data>,
    map: Arc<MapRepr>,
    metadata: Arc<Metadata>,
    decryptor: Arc<Decryptor>,
    has_ocgs: bool,
    trailer_data: TrailerData,
}

#[derive(Debug, Clone)]
enum Inner {
    Dummy,
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
    let mut visited = BTreeMap::new();
    populate_xref_impl_inner(data, pos, xref_map, &mut visited)
}

const MAX_XREF_SECTIONS: usize = 256;

enum SectionState<'a> {
    Active,
    Complete(&'a [u8]),
}

fn populate_xref_impl_inner<'a>(
    data: &'a [u8],
    pos: usize,
    xref_map: &mut XrefMap,
    visited: &mut BTreeMap<usize, (usize, SectionState<'a>)>,
) -> Option<&'a [u8]> {
    if let Some((&start, (end, state))) = visited.range(..=pos).next_back() {
        if start == pos {
            return match state {
                SectionState::Active => None,
                SectionState::Complete(trailer) => Some(trailer),
            };
        }
        if pos < *end {
            return None;
        }
    }
    if visited.len() >= MAX_XREF_SECTIONS {
        return None;
    }

    let limit = visited
        .range(pos..)
        .next()
        .map_or(data.len(), |(&start, _)| start);
    let mut reader = Reader::new_with(&data[..limit], pos);
    reader.skip_white_spaces_and_comments();

    let ctx = ReaderContext::dummy();
    let mut section = reader.clone();
    let stream = if section.read_without_context::<ObjectIdentifier>().is_some() {
        section.skip_white_spaces_and_comments();
        let (stream, extent) = Stream::read_with_extent(&mut section, &ctx)?;
        if limit < data.len() && extent > limit {
            return None;
        }
        Some((stream, extent))
    } else {
        None
    };
    let table_trailer;
    let (trailer, end) = if let Some((stream, end)) = &stream {
        (stream.dict(), *end)
    } else {
        table_trailer = read_xref_table_trailer(&mut section, &ctx)?;
        (&table_trailer, section.offset())
    };

    visited.insert(pos, (end, SectionState::Active));
    if let Some(prev) = trailer.get::<i32>(PREV) {
        populate_xref_impl_inner(data, prev as usize, xref_map, visited)?;
    }

    if let Some((stream, _)) = &stream {
        populate_from_xref_stream(stream, xref_map)?;
    } else {
        // In hybrid files, `XRefStm` entries override `Prev` ones.
        if let Some(xref_stm) = trailer.get::<i32>(XREF_STM) {
            populate_xref_impl_inner(data, xref_stm as usize, xref_map, visited)?;
        }
        populate_from_xref_table(&mut reader, xref_map)?;
    }

    let trailer = trailer.data();
    visited.insert(pos, (end, SectionState::Complete(trailer)));
    Some(trailer)
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

fn populate_from_xref_table(reader: &mut Reader<'_>, insert_map: &mut XrefMap) -> Option<()> {
    reader.skip_white_spaces();
    reader.forward_tag(b"xref")?;
    reader.skip_white_spaces();

    let mut max_obj = 0;

    while let Some(header) = reader.read_without_context::<SubsectionHeader>() {
        reader.skip_white_spaces();

        let start = header.start;
        let end = start.checked_add(header.num_entries)?;

        for obj_number in start..end {
            max_obj = max(max_obj, obj_number);
            let bytes = reader.read_bytes(XREF_ENTRY_LEN)?;
            crate::work::told(bytes.len());
            let entry = XRefEntry::read(bytes)?;

            // Entries beyond `Size` are not ignored.
            if entry.used {
                insert_map.insert(
                    ObjectIdentifier::new(obj_number, entry.gen_number),
                    EntryType::Normal(entry.offset),
                );
            }
        }
    }

    Some(())
}

fn populate_from_xref_stream(stream: &Stream<'_>, insert_map: &mut XrefMap) -> Option<()> {
    let size = stream.dict().get::<u32>(SIZE)?;

    let [f1_len, f2_len, f3_len] = stream.dict().get::<[u8; 3]>(W)?;

    if f2_len == 0 || f2_len > size_of::<u64>() as u8 {
        return None;
    }

    let xref_data = stream.decoded_data().ok()?;
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

    Some(())
}

fn xref_stream_num(data: &[u8]) -> Option<u32> {
    crate::work::told(data.len());
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
                return None;
            }
        }
        _n => {
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
            // TODO: Support a first-field width other than 1.
            xref_reader.read_bytes(1)?[0]
        };

        let obj_number = start.checked_add(i)?;

        match f_type {
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
                    ObjectIdentifier::new(obj_number, gen_number as i32),
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
                    ObjectIdentifier::new(obj_number, gen_number),
                    EntryType::ObjStream(obj_stream_number, index),
                );
            }
            _ => {
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
            vec![]
        };

        get(&encryption_dict, &id, password).map_err(XRefError::Encryption)
    } else {
        Ok(Decryptor::None)
    }
}

fn has_usable_object_stream_header(dict: &Dict<'_>) -> bool {
    dict.get::<usize>(N).is_some_and(|n| n > 0) && dict.get::<usize>(FIRST).is_some()
}

pub(crate) struct ObjectStream {
    pub(crate) next_byte: Option<u8>,
    data: Box<[u8]>,
    offsets: Vec<(u32, Option<usize>)>,
    by_number: crate::sync::OnceLock<Map<u32, u32>>,
}

impl ObjectStream {
    pub(crate) fn new(stream: &Stream<'_>) -> Option<Self> {
        let data = stream.decoded_data().ok()?.into_owned().into_boxed_slice();
        crate::work::bodies(data.len());
        let offsets = Self::read_offsets(stream, &data).unwrap_or_default();

        Some(Self {
            data,
            offsets,
            by_number: Default::default(),
            next_byte: None,
        })
    }

    fn read_offsets(stream: &Stream<'_>, data: &[u8]) -> Option<Vec<(u32, Option<usize>)>> {
        let num_objects = stream.dict().get::<usize>(N)?;
        let first_offset = stream.dict().get::<usize>(FIRST)?;

        let mut r = Reader::new(data);

        let mut offsets = vec![];
        let mut last_offset = None;

        for _ in 0..num_objects {
            r.skip_white_spaces_and_comments();
            let obj_num = r.read_without_context::<u32>()?;
            r.skip_white_spaces_and_comments();
            let relative_offset = r.read_without_context::<usize>()?;
            crate::work::entries(1);
            let offset = first_offset.saturating_add(relative_offset);

            // Offsets must strictly increase, or members could overlap.
            if offset >= data.len() || last_offset.is_some_and(|last| offset <= last) {
                offsets.push((obj_num, None));
            } else {
                let mut member = Reader::new(data);
                member.jump(offset);
                member.skip_white_spaces_and_comments();
                last_offset = Some(member.offset());
                offsets.push((obj_num, Some(member.offset())));
            }
        }

        Some(offsets)
    }

    fn member_index(&self, number: u32, listed: u32) -> Option<u32> {
        if self.obj_number(listed) == Some(number) {
            return Some(listed);
        }
        self.by_number
            .get_or_init(|| {
                self.offsets
                    .iter()
                    .zip(0u32..)
                    .map(|(&(n, _), i)| (n, i))
                    .collect()
            })
            .get(&number)
            .copied()
    }

    fn obj_number(&self, index: u32) -> Option<u32> {
        Some(self.offsets.get(index as usize)?.0)
    }

    fn get<'a, T>(&'a self, index: u32, ctx: &ReaderContext<'a>) -> Option<T>
    where
        T: Readable<'a>,
    {
        self.member(index, ctx, false)
    }
    fn member<'a, T: Readable<'a>>(
        &'a self,
        index: u32,
        ctx: &ReaderContext<'a>,
        persistent: bool,
    ) -> Option<T> {
        let id = ctx.obj_number().unwrap_or_else(|| {
            ObjectIdentifier::new(self.obj_number(index).unwrap_or_default(), 0)
        });
        let Some(offset) = self.offsets.get(index as usize)?.1 else {
            ctx.xref().note(Some(id));
            return None;
        };
        let end = self
            .offsets
            .get(index as usize + 1..)?
            .iter()
            .find_map(|e| e.1)
            .unwrap_or(self.data.len());
        let mut r = Reader::new_with(&self.data[..end], offset);
        let mut ctx = ctx.clone();
        ctx.set_in_object_stream(true);
        if persistent {
            ctx.mark_storage(r.tail()?);
        }
        read_bounded(
            &mut r,
            self.data.get(end).copied().or(self.next_byte),
            end,
            &ctx,
            id,
            false,
        )
    }
}

pub(crate) struct Carrier<'a>(pub(crate) Stream<'a>, pub(crate) Option<u8>);
impl<'a> Readable<'a> for Carrier<'a> {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let dict = r.read::<Dict<'a>>(ctx)?;
        if let Some((stream, _)) = Stream::read_after_dict(
            &mut r.clone(),
            dict.clone(),
            &crate::reader::Unknown,
            &mut NextEndstream::default(),
        ) {
            return Some(Self(stream, None));
        }
        let next_byte = ctx.cut()?;
        if dict.get::<Name<'_>>(TYPE).as_deref() != Some(b"ObjStm")
            || ctx.xref().needs_decryption(ctx)
            || dict.contains_key(b"Filter")
            || dict.contains_key(b"F")
        {
            return None;
        }
        object::stream::forward_stream_opener(r, &crate::reader::Unknown)?;
        let data = r.tail()?;
        Some(Self(Stream::new(data, dict), Some(next_byte)))
    }
}
fn read_bounded<'a, T: Readable<'a>>(
    r: &mut Reader<'a>,
    next_byte: Option<u8>,
    end: usize,
    ctx: &ReaderContext<'a>,
    id: ObjectIdentifier,
    indirect: bool,
) -> Option<T> {
    crate::work::body(|| {
        let start = r.offset();
        let object = if indirect {
            r.read_indirect::<T>(ctx)
        } else {
            r.read::<T>(ctx)
        };
        let split = next_byte.is_some_and(is_regular_character)
            && r.data.last().copied().is_some_and(is_regular_character);
        let cut = |pos: usize| pos == end && split;
        if let Some(o) = object {
            if !cut(r.offset()) {
                return Some(o);
            }
        } else {
            r.jump(start);
            if !T::HANDLES_TYPE_MISMATCH
                && Object::skip_indirect_value(r).is_some()
                && !cut(r.offset())
            {
                let mut suffix = r.clone();
                suffix.skip_white_spaces_and_comments();
                if !indirect
                    || suffix
                        .forward_tag(b"stream")
                        .and_then(|_| suffix.read_eol())
                        .is_none()
                {
                    return None;
                }
                r.jump(start);
                if r.read_indirect::<Object<'_>>(ctx).is_some() {
                    return None;
                }
            }
        }
        ctx.xref().note(Some(id));
        None
    })
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
mod tests {
    use super::*;

    #[test]
    fn rebuild_object_stream_offsets() {
        let pdf = b"1 0 obj null endobj\n2 0 obj\n\
            << /Type /ObjStm /N 3 /First 13 >> stream\n\
            1 0 4 5 5 99 null null\nendstream\nendobj";
        let data = PdfData::from(pdf.to_vec());
        let (map, _) = fallback_xref_map(&data, b"");
        let entry = |number| map.get(&ObjectIdentifier::new(number, 0));
        assert_eq!(entry(1), Some(&EntryType::Normal(0)));
        assert_eq!(entry(4), Some(&EntryType::ObjStream(2, 1)));
        assert_eq!(entry(5), None);
    }

    #[test]
    fn rebuild_object_stream_in_comment_before_cached_search() {
        // The ASCII85 payload starts with a comment delimiter and decodes to "\r4 0 null".
        let prefix = b"1 0 obj << /Type /ObjStm >>\n\
            % 2 0 obj << /Type /ObjStm /N 1 /First 5 /Filter /ASCII85Decode >> stream\n\
            %5]1]+Du=<C] endstream\n\
            stream\n|";
        for suffix in [b"".as_slice(), b" endstream"] {
            let mut bytes = prefix.to_vec();
            bytes.extend_from_slice(suffix);
            let (map, _) = fallback_xref_map(&PdfData::from(bytes), b"");
            assert_eq!(
                map.get(&ObjectIdentifier::new(4, 0)),
                Some(&EntryType::ObjStream(2, 0))
            );
        }
    }

    #[test]
    fn rebuild_entries_and_trailer_match_object_reads() {
        for depth in [511, 512, 513] {
            let trailer = b"<< /Root 2 0 R >>";
            let catalog = b"<< /Type /Catalog /Pages 3 0 R >>";
            let mut bytes = b"1 0 obj ".to_vec();
            let body = bytes.len();
            bytes.extend(core::iter::repeat_n(b'[', depth));
            bytes.extend(core::iter::repeat_n(b']', depth));
            bytes.extend_from_slice(b" endobj\n<< /Broken [\n");
            let catalog_offset = bytes.len();
            bytes.extend_from_slice(b"2 0 obj ");
            bytes.extend_from_slice(catalog);
            bytes.extend_from_slice(b" endobj\n");
            let pages_offset = bytes.len();
            bytes.extend_from_slice(b"3 0 obj << /Kids [] /Count 0 >> endobj\n");

            let mut expected = XrefMap::default();
            if Reader::new_with(&bytes, body)
                .skip::<Object<'_>>(false)
                .is_some()
            {
                expected.insert(ObjectIdentifier::new(1, 0), EntryType::Normal(0));
            }
            let root = ObjectIdentifier::new(2, 0);
            expected.insert(root, EntryType::Normal(catalog_offset));
            expected.insert(ObjectIdentifier::new(3, 0), EntryType::Normal(pages_offset));

            for with_trailer in [false, true] {
                if with_trailer {
                    bytes.extend_from_slice(trailer);
                }
                let data = bytes.clone().into();
                let (entries, input) = fallback_xref_map(&data, b"");
                assert_eq!(entries, expected);
                if with_trailer {
                    assert!(matches!(input, Some(XRefInput::TrailerDictData(d)) if d == trailer));
                } else {
                    assert!(matches!(input, Some(XRefInput::RootRef(id)) if id == root));
                }
            }
        }
    }

    #[test]
    fn damaged_ascii_hex_object_stream() {
        let mut reader = Reader::new(
            b"<< /Type /ObjStm /N 1 /First 4 /Length 13 /Filter /ASCIIHexDecode >>\n\
              stream\n3520302031.2>\nendstream",
        );
        let stream = reader
            .read_indirect::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();
        assert!(ObjectStream::new(&stream).is_none());
    }

    #[test]
    fn damaged_ascii_hex_xref_triggers_repair() {
        let mut pdf = b"%PDF-1.5\n".to_vec();
        let catalog = pdf.len();
        pdf.extend_from_slice(b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
        let pages = pdf.len();
        pdf.extend_from_slice(b"2 0 obj << /Type /Pages /Kids [] /Count 0 >> endobj\n");
        pdf.extend_from_slice(b"3 0 obj 42 endobj\n");
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!(
                "4 0 obj << /Type /XRef /Size 5 /Root 1 0 R /Index [1 2] /W [0 1 0] \
             /Length 6 /Filter /ASCIIHexDecode >>\nstream\n{catalog:02X}{pages:02X}.>\n\
             endstream\nendobj\nstartxref\n{xref}\n%%EOF"
            )
            .as_bytes(),
        );

        let pdf = crate::Pdf::new(pdf).unwrap();
        assert_eq!(pdf.xref().get::<i32>(ObjectIdentifier::new(3, 0)), Some(42));
    }

    #[test]
    fn rebuild_nested_broken_families_match_expected_entries_and_trailer() {
        const N: usize = 1024;
        let obj_spaced_array = (1..=N).map(|i| format!("{i} 0 obj [ ")).collect::<String>();
        let obj_adjacent_array = (1..=N).map(|i| format!("{i} 0 obj[ ")).collect::<String>();
        let obj_spaced_dict = (1..=N)
            .map(|i| format!("{i} 0 obj << "))
            .collect::<String>();
        let obj_adjacent_dict = (1..=N).map(|i| format!("{i} 0 obj<< ")).collect::<String>();
        let families = [
            ("dict_key_nested", "<< /A ".repeat(N)),
            ("dict_array_nested", "<< /A [ ".repeat(N)),
            ("spaced_dicts", "<< ".repeat(N)),
            ("obj_spaced_array", obj_spaced_array),
            ("obj_adjacent_array", obj_adjacent_array),
            ("obj_spaced_dict", obj_spaced_dict),
            ("obj_adjacent_dict", obj_adjacent_dict),
        ];

        let trailer = b"<< /Root 9001 0 R >>";
        for (name, prefix) in families {
            let mut bytes = prefix.into_bytes();
            bytes.push(b'\n');
            let catalog_offset = bytes.len();
            bytes.extend_from_slice(b"9001 0 obj << /Type /Catalog /Pages 9002 0 R >> endobj\n");
            let pages_offset = bytes.len();
            bytes.extend_from_slice(b"9002 0 obj << /Kids [] /Count 0 >> endobj\n");

            let root = ObjectIdentifier::new(9001, 0);
            let pages = ObjectIdentifier::new(9002, 0);
            let mut expected = XrefMap::default();
            expected.insert(root, EntryType::Normal(catalog_offset));
            expected.insert(pages, EntryType::Normal(pages_offset));

            for with_trailer in [false, true] {
                if with_trailer {
                    bytes.extend_from_slice(trailer);
                }
                let data = bytes.clone().into();
                let (entries, input) = fallback_xref_map(&data, b"");
                assert_eq!(entries, expected, "{name} (with_trailer={with_trailer})");
                if with_trailer {
                    assert!(
                        matches!(input, Some(XRefInput::TrailerDictData(d)) if d == trailer),
                        "{name}"
                    );
                } else {
                    assert!(
                        matches!(input, Some(XRefInput::RootRef(id)) if id == root),
                        "{name}"
                    );
                }
            }
        }
    }

    #[test]
    fn rebuild_selects_last_trailer_with_valid_root() {
        for catalog in [
            &b"1 0 obj << /Pages 2 0 R >> endobj\n4 0 obj null endobj"[..],
            b"5 0 obj << /Type /ObjStm /N 2 /First 9 /Length 32 >>\nstream\n1 0 4 19 << /Pages 2 0 R >> null\nendstream\nendobj",
        ] {
            let data: PdfData = [
                catalog,
                b"\n2 0 obj << /Type /Pages >> endobj\n",
                b"trailer << /Root 1 0 R >>\n",
                b"trailer << /Root 1 0 R /Size 6 >>\n",
                b"trailer << /Root 4 0 R >>\n",
                b"trailer << /Root 2 0 R >>\n",
                b"trailer << /Root 4 0 R >>\n",
                b"trailer << /Root 2 0 R >>\n",
                b"1 1 obj null endobj\ntrailer << /Root 1 1 R >>",
            ]
            .concat()
            .into();
            let (_, Some(XRefInput::TrailerDictData(bytes))) = fallback_xref_map(&data, b"") else {
                panic!("expected a trailer dictionary");
            };
            let trailer = Reader::new(bytes).read_without_context::<Dict<'_>>().unwrap();
            assert_eq!(trailer.get::<u32>(SIZE), Some(6));
        }
    }

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

        let mut xref_map = Map::default();
        let xref_pos = find_last_xref_pos(pdf.as_ref()).unwrap();
        assert_eq!(
            populate_xref_impl(pdf.as_ref(), xref_pos, &mut xref_map),
            None
        );
    }

    #[test]
    fn overlapping_xref_ranges() {
        let table = b" xref\ntrailer\n<<>>";
        let data = table.repeat(3);
        let middle = table.len();
        for (first, second) in [(middle, middle + 1), (middle + 1, middle)] {
            let mut sections = BTreeMap::new();
            let mut map = XrefMap::default();
            for pos in [0, 2 * middle, first] {
                populate_xref_impl_inner(&data, pos, &mut map, &mut sections).unwrap();
            }
            assert!(populate_xref_impl_inner(&data, second, &mut map, &mut sections).is_none());
        }
    }

    #[test]
    fn prev_inside_active_range() {
        let active = b" xref\ntrailer\n<<>>";
        let data = [&active[..], b"xref\ntrailer\n<< /Prev 1 >>"].concat();
        let mut sections = BTreeMap::from([(0, (active.len(), SectionState::Active))]);
        let mut map = XrefMap::default();
        assert!(populate_xref_impl_inner(&data, active.len(), &mut map, &mut sections).is_none());
    }

    #[test]
    fn completed_xref_at_section_limit() {
        let first = b"xref\n1 1\n0000000010 00000 n \ntrailer\n<< /Size 2 >>\n";
        let second = b"xref\n1 1\n0000000020 00000 n \ntrailer\n<< /Size 3 >>\n";
        let empty = b"xref\ntrailer\n<<>>\n";
        let mut data = [first.as_slice(), second.as_slice()].concat();
        let start = data.len();
        data.extend(empty.repeat(MAX_XREF_SECTIONS - 1));
        let mut visited = BTreeMap::new();
        let mut map = XrefMap::default();
        populate_xref_impl_inner(&data, 0, &mut map, &mut visited).unwrap();
        populate_xref_impl_inner(&data, first.len(), &mut map, &mut visited).unwrap();
        for i in 0..MAX_XREF_SECTIONS - 2 {
            populate_xref_impl_inner(&data, start + i * empty.len(), &mut map, &mut visited)
                .unwrap();
        }
        assert_eq!(
            populate_xref_impl_inner(&data, 0, &mut map, &mut visited),
            Some(b"<< /Size 2 >>".as_slice())
        );
        assert_eq!(map[&ObjectIdentifier::new(1, 0)], EntryType::Normal(20));
        assert!(
            populate_xref_impl_inner(&data, data.len() - empty.len(), &mut map, &mut visited)
                .is_none()
        );
    }

    #[test]
    fn xref_stream_length_crosses_table_start() {
        let payload_and_tail = b"\x01\x05\nendstream\nendobj\n";
        for length in [payload_and_tail.len(), payload_and_tail.len() + 1] {
            let mut data = format!("1 0 obj\n<< /Size 1 /W [1 1 0] /Length {length} >>\nstream\n")
                .into_bytes();
            data.extend_from_slice(payload_and_tail);
            let table = data.len();
            data.extend_from_slice(b"xref\ntrailer\n<< /XRefStm 0 >>");
            assert!(populate_xref_impl(&data, table, &mut XrefMap::default()).is_none());
        }
    }

    #[test]
    fn adjacent_xref_sections() {
        let stream = b"1 0 obj\n<< /Size 0 /W [1 1 0] /Length 0 >>\nstream\n\nendstream";
        let data = [&stream[..], b" % next section\nxref\ntrailer\n<<>>"].concat();
        for (first, second) in [(0, stream.len()), (stream.len(), 0)] {
            let mut sections = BTreeMap::new();
            let mut map = XrefMap::default();
            assert!(populate_xref_impl_inner(&data, first, &mut map, &mut sections).is_some());
            assert!(populate_xref_impl_inner(&data, second, &mut map, &mut sections).is_some());
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

    #[test]
    fn xref_stream_subsection_rejects_overflowing_object_number() {
        let data = [1, 0, 1, 0];
        let mut xref_map = Map::default();
        let result =
            xref_stream_subsection(&mut Reader::new(&data), u32::MAX, 2, 1, 1, 0, &mut xref_map);
        assert!(result.is_none());
        assert!(xref_map.contains_key(&ObjectIdentifier::new(u32::MAX, 0)));
    }
}
