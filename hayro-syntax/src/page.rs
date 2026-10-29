//! Reading the pages of a PDF document.

use crate::content::{TypedIter, UntypedIter};
use crate::hash;
use crate::lost::{Lost, Memo};
use crate::object::Array;
use crate::object::Dict;
use crate::object::Name;
use crate::object::Rect;
use crate::object::Stream;
use crate::object::dict::keys::*;
use crate::object::stream::{DecodeFailure, ReadError, StrictReader};
use crate::object::{MaybeRef, Null, Object, ObjectIdentifier};
use crate::reader::ReaderContext;
use crate::sync::OnceLock;
use crate::transform::Transform;
use crate::util::FloatExt;
use crate::work::{Meter, POLL_BYTES};
use crate::xref::{ContentKey, XRef};
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Deref;

mod inherit;
mod tree;

pub use inherit::{Inheritable, Inheritance, InheritanceError};

/// The pages of a PDF document, in order, each with the entries it inherits.
///
/// # Which pages, in what order
///
/// - The root of the page tree is the catalog's /Pages, a reference or a dictionary
///   written into the catalog. Wherever a page tree entry is read, a stream counts
///   as its dictionary.
/// - A page tree entry is a node, whose kids are listed in its place, when its /Type
///   is the name /Pages, read through a reference if need be. An entry without /Type
///   is a node when it has /Kids and no `/MediaBox`. For both tests a direct `null`
///   value is no entry, while any other value, a reference to nothing included, is
///   one. Every other entry is a page: /Type /Page or another name, a /Type that is
///   no name, and an entry that is no dictionary at all.
/// - Pages are listed depth first, each node's kids in the order of its /Kids. A
///   /Kids that is no array, or a reference to nothing, lists nothing.
/// - /Count is never read: a damaged /Count hides no page the /Kids hold and adds
///   none. `PyMuPDF` 1.28.2 numbers pages by /Count, so on a damaged tree this lists
///   more pages than it does, deliberately.
/// - A node lists its kids the first time the walk meets it. Met again — listed
///   twice, by two nodes, in a /Kids cycle, or by itself — it lists nothing more, and
///   the walk goes on after it. A page is listed each time a node lists it. Nodes
///   are told apart by object number and generation, and a dictionary written into a
///   /Kids array by where it is in the file, so that it is one node however many
///   nodes list the array holding it.
/// - A /Kids entry that is no dictionary — a reference to nothing or to an object
///   that is no dictionary, or a number, name, string, array or `null` written into
///   the array — is a page with no entries ([`Page::raw`] is empty). A page's object
///   ([`Page::obj_id`]) is the one its /Kids entry refers to, none for one written
///   into the array.
/// - A root that is no node is the document's one page; an empty dictionary is one.
/// - Where the catalog has no /Pages, or one that is no dictionary or a reference to
///   nothing, or the tree lists no page, the pages are every dictionary of the file,
///   streams excepted, that has /Contents, whatever its value, and has no /Type, a
///   /Type that is no name, or /Type /Page; in the order of the objects in the file,
///   an object of an object stream at its stream's place. Such a page inherits
///   through its /Parent alone.
/// - Where a damaged file holds two generations of one object number, each is an
///   object of its own: two nodes, or two pages.
/// - Listing the pages and reading what they inherit takes time linear in the
///   objects of the tree and its listings, however the tree shares nodes and /Kids
///   arrays, and stack that does not grow with its depth.
///
/// # What each page inherits
///
/// Each page's media box, crop box, rotation and resources are found by
/// [`Inheritance`] lookups made page by page, for each page in the order `/MediaBox`,
/// `/CropBox`, /Rotate, /Resources, and, where one of them met a /Parent loop, made
/// again: so every page reads the repaired /Parent entries if any page met a loop,
/// as `PyMuPDF` 1.28.2 shows once every page is loaded. A lookup that fails on a
/// loop finds no value.
///
/// - A box is the first four items of an array, each a number or a reference to
///   one, read at single precision, its corners in either order; anything else is no
///   box. Without a media box, the page is A4 ([`A4`]); without a crop box, the crop
///   box is the media box. The crop box is not cut to the media box.
/// - The rotation is the integer part of a number, turned into 0 to 359; one that
///   is no multiple of 90, does not fit 32 bits, or is no number is none.
/// - The resources are a dictionary, anything else, a stream included, being none
///   (`PyMuPDF` 1.28.2 reads a stream's dictionary there).
///   Each category of them (`/ExtGState`, /Font, /Properties, `/ColorSpace`, `/XObject`,
///   /Pattern, /Shading) is the dictionary its entry is or refers to, or a stream's
///   dictionary, even where the stream's data cannot be read; else it is empty.
///
/// # What cannot be read
///
/// An object the file lists but that cannot be read reads as `null`.
/// [`Self::unreadable`] names the first one the walk of the tree met: the root, a
/// /Kids array, a /Kids entry (which is then a page with no entries) or an entry's
/// /Type; where the pages are found without the tree, the first the scan met, which
/// reads the objects in file order and, with each, what its /Type refers to. What a page
/// inherits is not the document's: [`Page::unreadable`] names the first object its
/// lookups met (a value, or a /Parent on the way), then the first of its boxes'
/// numbers and its resources' categories, each told to every page read with it,
/// though it is read once for them all.
pub struct Pages<'a> {
    pages: Vec<Page<'a>>,
    xref: &'a XRef,
    /// What reading the page tree lost.
    lost: Lost,
    /// The root of the page tree, which [`Inheritance`] looks up entries below.
    root: Option<tree::Key<'a>>,
}

impl<'a> Pages<'a> {
    // The pages the page tree lists, depth first; where the catalog has no page tree
    // that lists a page, every page object of the file, in object order. The walk
    // ignores /Count: it hides no page the /Kids hold.
    //
    // What reading the tree loses — a node, a /Kids array or a node's /Type that
    // cannot be read, or, where no tree lists a page, any object — may have held
    // pages: it is kept for the document ([`Self::unreadable`]). What reading a
    // page's entries loses is kept for the page ([`Page::unreadable`]). Neither is
    // told to a computation being watched while the pages are read.
    pub(crate) fn new(ctx: &ReaderContext<'a>, xref: &'a XRef) -> Option<Self> {
        let (walked, walk_lost) =
            xref.apart(|| tree::root(xref).map(|(id, root)| tree::walk(xref, id, root)));
        let ((tree, listed), lost) = match walked {
            Some((tree, listed)) if !listed.is_empty() => ((tree, listed), walk_lost),
            _ => {
                let (scanned, lost) = xref.apart(|| tree::scan(xref));
                let lost = lost.or(xref.unreadable_object());
                ((tree::Tree::default(), scanned), lost)
            }
        };
        let pages = Inherited::find(xref, &tree, &listed)
            .into_iter()
            .zip(listed)
            .map(|((inherited, unreadable), listed)| Page::new(listed, inherited, unreadable, ctx))
            .collect();
        Some(Self {
            pages,
            xref,
            lost,
            root: tree.root,
        })
    }

    /// An object the page tree refers to that the file lists but that cannot be read,
    /// if there is one: a node, a /Kids array, or a node's /Type, which may have held
    /// pages, or, where no page tree lists a page and every page object of the file
    /// is a page, any object. The pages are those that can be read, as viewers list
    /// them, and a copy of all of them may lose some.
    pub fn unreadable(&self) -> Option<ObjectIdentifier> {
        self.lost
    }

    /// Return the xref table (of the document the pages belong to).   
    pub fn xref(&self) -> &'a XRef {
        self.xref
    }
}

impl<'a> Deref for Pages<'a> {
    type Target = [Page<'a>];

    fn deref(&self) -> &Self::Target {
        &self.pages
    }
}

/// A part of a page's /Contents that cannot be read; the object is `None` for a part
/// written directly into the page or its /Contents array.
#[derive(Debug, Copy, Clone)]
pub enum ContentError {
    /// The part is no stream, or its object cannot be parsed.
    Unreadable(Option<ObjectIdentifier>),
    /// The part's stream cannot be decrypted or decoded.
    Decode(Option<ObjectIdentifier>, DecodeFailure),
}

/// Identifies what a page draws and where, for a cache of what is copied or drawn of
/// pages: its dictionary's bytes ([`Dict::content_key`]), which hold its content and
/// what else it names, the resource categories it is drawn with, its own or
/// inherited, its media and crop boxes and its rotation. Pages the page tree lists
/// with one dictionary, under the same inherited resources and boxes, have equal
/// keys, so that one copy of what they draw serves them all.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct PageKey<'a> {
    dict: ContentKey<'a>,
    resources: [ContentKey<'a>; 7],
    boxes: [u64; 8],
    rotation: u8,
}

/// The decoded data of each body an array of contents names, decoded once however
/// many times it names it.
#[derive(Default)]
struct Decoded<'a>(hash::Map<ContentKey<'a>, Option<Vec<u8>>>, Option<Vec<u8>>);

impl<'a> Decoded<'a> {
    /// The data of the body `key` names, `decode`d the first time it is asked for;
    /// a part with no key is decoded each time.
    fn get(
        &mut self,
        key: Option<ContentKey<'a>>,
        decode: impl FnOnce() -> Option<Vec<u8>>,
    ) -> Option<&[u8]> {
        let Some(key) = key else {
            self.1 = decode();
            return self.1.as_deref();
        };
        self.0.entry(key).or_insert_with(decode).as_deref()
    }
}

/// The most decoded bytes of the streams an array of contents names more than once
/// [`Page::try_page_stream_to`] keeps, to write them again rather than decode them
/// again; those of streams that decode to more are decoded each time they are named.
const CONTENT_KEPT: usize = 1 << 20;

/// The most decoded bytes of a stream [`Page::try_page_stream_to`] holds before it
/// writes them, once the content restarted; a stream that decodes to more is read
/// to its end, then again as it is written.
const CONTENT_HELD: usize = 1 << 20;

/// The data `reader` reads, where they are at most [`CONTENT_HELD`] bytes; `None`
/// once it read more to their end, so that, rewound, it reads them with no restart.
fn held(
    reader: &mut StrictReader<'_>,
    buf: &mut [u8],
    meter: &Meter<'_>,
) -> Result<Option<Vec<u8>>, DecodeFailure> {
    let mut data = Some(Vec::new());
    loop {
        match reader.read(buf, meter) {
            Ok(0) => return Ok(data),
            Ok(n) => match &mut data {
                Some(d) if d.len() + n <= CONTENT_HELD => d.extend_from_slice(&buf[..n]),
                _ => data = None,
            },
            Err(ReadError::Restarted) => data = Some(Vec::new()),
            Err(error) => return Err(failure(error)),
        }
    }
}

/// Why decoding stopped, where reading it did for `error`.
fn failure(error: ReadError) -> DecodeFailure {
    match error {
        ReadError::Cancelled => DecodeFailure::Cancelled,
        ReadError::Restarted | ReadError::Failed => DecodeFailure::StreamDecode,
    }
}

/// A piece of a page's content, written as it is decoded
/// ([`Page::try_page_stream_to`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentPiece<'d> {
    /// The next bytes of the content.
    Data(&'d [u8]),
    /// Every piece written before is void, and the content is written again from its
    /// start: a stream turned out corrupt for the decoder that read it, and is read
    /// with the next one decoding whole would try. At most once for a content.
    Restart,
}

/// The rotation of the page.
#[derive(Debug, Copy, Clone)]
pub enum Rotation {
    /// No rotation.
    None,
    /// A rotation of 90 degrees.
    Horizontal,
    /// A rotation of 180 degrees.
    Flipped,
    /// A rotation of 270 degrees.
    FlippedHorizontal,
}

/// What a page inherits, as [`Page`] holds it.
struct Inherited<'a> {
    media_box: Option<Rect>,
    crop_box: Option<Rect>,
    rotation: Rotation,
    resources: Resources<'a>,
}

impl<'a> Inherited<'a> {
    /// What each page of `listed` inherits, and the first object reading it lost:
    /// each page looked up in order, and every page again where a lookup met a
    /// /Parent loop, so that every page reads the repaired /Parent entries. A box,
    /// and the categories of resources, are read once for every page read with them.
    fn find(
        xref: &'a XRef,
        tree: &tree::Tree<'a>,
        listed: &[tree::Listed<'a>],
    ) -> Vec<(Self, Lost)> {
        let mut lookups = inherit::Lookups::new(xref, tree.root, Some(tree));
        let mut boxes = Memo::<ContentKey<'a>, Option<Rect>>::default();
        let mut resources = Memo::<ContentKey<'a>, Resources<'a>>::default();
        let mut each = |lookups: &mut inherit::Lookups<'_, 'a>| -> Vec<(Self, Lost)> {
            listed
                .iter()
                .map(|page| {
                    let start = inherit::Start {
                        key: tree::Key::of(page.id, &page.dict),
                        dict: &page.dict,
                        under: page.under,
                    };
                    xref.apart(|| {
                        let [media_box, crop_box, rotate, found] =
                            Inheritable::ALL.map(|key| lookups.get(&start, key).ok().flatten());
                        let mut rect = |value: Option<Object<'a>>| match value {
                            Some(Object::Array(array)) => {
                                *boxes.get(xref, array.content_key(), || {
                                    Rect::try_from(Object::Array(array.clone())).ok()
                                })
                            }
                            _ => None,
                        };
                        let media_box = rect(media_box);
                        let crop_box = rect(crop_box);
                        let resources = match found {
                            Some(Object::Dict(dict)) => resources
                                .get(xref, dict.content_key(), || {
                                    Resources::read(|key| tree::value(xref, &dict, key))
                                })
                                .clone(),
                            _ => Resources::new(Dict::empty()),
                        };
                        Self {
                            media_box,
                            crop_box,
                            rotation: rotation(rotate),
                            resources,
                        }
                    })
                })
                .collect()
        };
        let found = each(&mut lookups);
        if lookups.repaired() {
            each(&mut lookups)
        } else {
            found
        }
    }
}

/// The rotation a /Rotate value gives: the integer part of a number that fits 32
/// bits, turned into 0 to 359, where it is a multiple of 90; else none.
fn rotation(value: Option<Object<'_>>) -> Rotation {
    let Some(Object::Number(number)) = value else {
        return Rotation::None;
    };
    let Ok(degrees) = i32::try_from(number.as_i64()) else {
        return Rotation::None;
    };
    match degrees.rem_euclid(360) {
        90 => Rotation::Horizontal,
        180 => Rotation::Flipped,
        270 => Rotation::FlippedHorizontal,
        _ => Rotation::None,
    }
}

/// A PDF page.
pub struct Page<'a> {
    id: Option<ObjectIdentifier>,
    inner: Dict<'a>,
    media_box: Rect,
    crop_box: Rect,
    rotation: Rotation,
    /// The content, and what reading it lost.
    page_streams: OnceLock<(Option<Vec<u8>>, Lost)>,
    resources: Resources<'a>,
    unreadable: Lost,
    ctx: ReaderContext<'a>,
    /// The node of the page tree that lists the page, which it inherits from where
    /// its /Parent chain ends without an entry ([`Inheritance`]).
    under: Option<u32>,
}

impl<'a> Page<'a> {
    fn new(
        listed: tree::Listed<'a>,
        inherited: Inherited<'a>,
        unreadable: Lost,
        reader_ctx: &ReaderContext<'a>,
    ) -> Self {
        let media_box = inherited.media_box.unwrap_or(A4);
        let crop_box = inherited.crop_box.unwrap_or(media_box);

        Self {
            id: listed.id,
            inner: listed.dict,
            media_box,
            crop_box,
            rotation: inherited.rotation,
            page_streams: OnceLock::new(),
            resources: inherited.resources,
            unreadable,
            ctx: reader_ctx.clone(),
            under: listed.under,
        }
    }

    /// What the page draws and where ([`PageKey`]).
    pub fn key(&self) -> PageKey<'a> {
        let r = &self.resources;
        let corners = |rect: Rect| [rect.x0, rect.y0, rect.x1, rect.y1].map(f64::to_bits);
        let [m0, m1, m2, m3] = corners(self.media_box);
        let [c0, c1, c2, c3] = corners(self.crop_box);
        PageKey {
            dict: self.inner.content_key(),
            resources: [
                &r.ext_g_states,
                &r.fonts,
                &r.properties,
                &r.color_spaces,
                &r.x_objects,
                &r.patterns,
                &r.shadings,
            ]
            .map(Dict::content_key),
            boxes: [m0, m1, m2, m3, c0, c1, c2, c3],
            rotation: self.rotation as u8,
        }
    }

    /// The page object's identifier; `None` for a page written directly into a /Kids
    /// array. A /Kids entry that is not a dictionary is a page with no entries.
    pub fn obj_id(&self) -> Option<ObjectIdentifier> {
        self.id
    }

    fn operations_impl(&self) -> Option<UntypedIter<'_>> {
        let stream = self.page_stream()?;
        let iter = UntypedIter::new(stream);

        Some(iter)
    }

    /// Return the decoded content stream of the page, kept with the page for later
    /// calls; what reading it lost is told to the computation being watched each
    /// time ([`crate::lost`]). A stream an array of contents names more than once is
    /// decoded once.
    pub fn page_stream(&self) -> Option<&[u8]> {
        let (data, lost) = self
            .page_streams
            .get_or_init(|| self.xref().watch(|| self.read_page_stream()));
        self.xref().note(*lost);
        data.as_deref()
    }

    /// The content [`Self::page_stream`] returns, decoded anew at each call and not
    /// kept: for a reader that keeps the document open while it draws page after
    /// page, which would otherwise hold every page's decoded content for as long as
    /// the document is open. What reading it lost is told to the computation being
    /// watched.
    pub fn read_page_stream(&self) -> Option<Vec<u8>> {
        if let Some(stream) = self.inner.get::<Stream<'_>>(CONTENTS) {
            Some(stream.decoded().ok()?.into_owned())
        } else if let Some(array) = self.inner.get::<Array<'_>>(CONTENTS) {
            let mut decoded = Decoded::default();
            let mut collected = vec![];
            // A part that is no stream is passed over, as `MuPDF` passes it
            // over (`PyMuPDF` 1.28.2, observed): a null, a number, a dictionary,
            // a reference to none.
            for item in array.raw_iter() {
                let key = self.body_key(&item);
                let stream = match item {
                    MaybeRef::Ref(r) => self.xref().get_with::<Stream<'_>>(r.into(), array.ctx()),
                    MaybeRef::NotRef(object) => object.into_stream(),
                };
                let Some(stream) = stream else {
                    continue;
                };
                if let Some(data) = decoded.get(key, || Some(stream.decoded().ok()?.into_owned())) {
                    collected.extend_from_slice(data);
                    // Streams must have at least one whitespace in-between.
                    collected.push(b' ');
                }
            }

            Some(collected)
        } else {
            warn!("contents entry of page was neither stream nor array of streams");

            None
        }
    }

    /// The decoded content of the page, as [`Self::page_stream`] joins it, or the
    /// first part that cannot be read: unlike `page_stream`, which leaves such a part
    /// out, so that a page that lost content is not taken for a whole one. A missing
    /// or `null` /Contents, or a part that is `null`, is no content. See
    /// [`Self::try_page_stream_to`].
    pub fn try_page_stream(&self) -> Result<Vec<u8>, ContentError> {
        let mut content = vec![];
        self.try_page_stream_to(
            &mut |piece| match piece {
                ContentPiece::Data(data) => content.extend_from_slice(data),
                ContentPiece::Restart => content.clear(),
            },
            &Meter::default(),
        )?;
        Ok(content)
    }

    /// [`Self::try_page_stream`], written to `out` a piece at a time as it is decoded,
    /// the work counted on `meter`: decoding holds the decoders' state and windows
    /// and buffers of [`POLL_BYTES`], however much the content's filters make of its
    /// data and however they nest, and up to 1 MiB of the streams an array of
    /// contents names more than once, each decoded once if it fits. The pieces after
    /// the [`ContentPiece::Restart`], if there is one, are the content: once it
    /// restarted, each stream is held, up to 1 MiB, or read twice, before it is
    /// written. On an error, `out` was given part of the content.
    pub fn try_page_stream_to(
        &self,
        out: &mut dyn FnMut(ContentPiece<'_>),
        meter: &Meter<'_>,
    ) -> Result<(), ContentError> {
        let Some(contents) = self.inner.get_raw::<Object<'a>>(CONTENTS) else {
            return Ok(());
        };
        let resolved = |object: &MaybeRef<Object<'a>>| -> Result<_, ContentError> {
            match object {
                MaybeRef::NotRef(object) => Ok((None, object.clone())),
                MaybeRef::Ref(r) => {
                    let id = ObjectIdentifier::from(*r);
                    match self
                        .ctx
                        .xref()
                        .get_checked_with::<Object<'a>>(id, &self.ctx)
                    {
                        Ok(object) => Ok((Some(id), object.unwrap_or(Object::Null(Null)))),
                        Err(id) => Err(ContentError::Unreadable(Some(id))),
                    }
                }
            }
        };
        // The parts, and whether each is followed by a space, as streams of an array
        // must have at least one whitespace in-between.
        let (parts, spaced) = match resolved(&contents)? {
            (_, Object::Array(array)) => (array.raw_iter().collect::<Vec<_>>(), true),
            _ => (vec![contents], false),
        };
        let keys: Vec<_> = parts.iter().map(|part| self.body_key(part)).collect();
        let mut named = hash::Map::<ContentKey<'a>, usize>::default();
        for key in keys.iter().flatten() {
            *named.entry(*key).or_default() += 1;
        }
        // The data of parts named more than once, `None` for a `null` one.
        let mut kept = hash::Map::<ContentKey<'a>, Option<Vec<u8>>>::default();
        let mut kept_bytes = 0;
        let mut buf = vec![0; POLL_BYTES];
        // Whether the content restarted.
        let mut careful = false;
        'content: loop {
            let mut written = false;
            for (part, key) in parts.iter().zip(&keys) {
                let (id, object) = resolved(part)?;
                if let Some(data) = key.and_then(|key| kept.get(&key)) {
                    if let Some(data) = data {
                        out(ContentPiece::Data(data));
                        out(ContentPiece::Data(b" "));
                        written = true;
                    }
                    continue;
                }
                let stream = match object {
                    Object::Null(_) => {
                        if let Some(key) = key {
                            kept.insert(*key, None);
                        }
                        continue;
                    }
                    Object::Stream(stream) => stream,
                    _ => return Err(ContentError::Unreadable(id)),
                };
                let failed = |failure| ContentError::Decode(id, failure);
                let repeated = key.is_some_and(|key| named[&key] > 1);
                let room = if repeated {
                    CONTENT_KEPT - kept_bytes
                } else {
                    0
                };
                let mut copy = (room > 0).then(Vec::new);
                let keep = |copy: &mut Option<Vec<u8>>, data: &[u8]| match copy {
                    Some(c) if c.len() + data.len() <= room => c.extend_from_slice(data),
                    _ => *copy = None,
                };
                match stream.decoded_strict_reader(meter).map_err(failed)? {
                    None => {
                        let data = stream.decoded_strict_metered(meter).map_err(failed)?;
                        out(ContentPiece::Data(&data));
                        written = true;
                        keep(&mut copy, &data);
                    }
                    // After a restart, each part is read to its end before it is
                    // written, so that the content restarts once: written again
                    // each time, the parts before would make it quadratic.
                    Some(mut reader) if careful => {
                        let held = held(&mut reader, &mut buf, meter).map_err(failed)?;
                        if let Some(data) = held {
                            out(ContentPiece::Data(&data));
                            keep(&mut copy, &data);
                        } else {
                            reader.rewind();
                            loop {
                                match reader.read(&mut buf, meter) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        out(ContentPiece::Data(&buf[..n]));
                                        keep(&mut copy, &buf[..n]);
                                    }
                                    // A reading to the end is read again with no restart.
                                    Err(error) => return Err(failed(failure(error))),
                                }
                            }
                        }
                    }
                    Some(mut reader) => loop {
                        match reader.read(&mut buf, meter) {
                            Ok(0) => break,
                            Ok(n) => {
                                out(ContentPiece::Data(&buf[..n]));
                                written = true;
                                keep(&mut copy, &buf[..n]);
                            }
                            Err(ReadError::Restarted) if written => {
                                out(ContentPiece::Restart);
                                careful = true;
                                continue 'content;
                            }
                            Err(ReadError::Restarted) => {
                                if let Some(c) = &mut copy {
                                    c.clear();
                                }
                            }
                            Err(error) => return Err(failed(failure(error))),
                        }
                    },
                }
                if spaced {
                    out(ContentPiece::Data(b" "));
                    written = true;
                }
                if let (Some(key), Some(copy)) = (key, copy) {
                    kept_bytes += copy.len();
                    kept.insert(*key, Some(copy));
                }
            }
            return Ok(());
        }
    }

    /// The body a part of an array of contents leads to, where it is a reference.
    fn body_key<T>(&self, item: &MaybeRef<T>) -> Option<ContentKey<'a>> {
        match item {
            MaybeRef::Ref(r) => self.ctx.xref().content_key((*r).into()),
            MaybeRef::NotRef(_) => None,
        }
    }

    /// An object the page's boxes, rotation or resources were read with that the
    /// file lists but that cannot be read, if there is one ([`crate::lost`]): an entry
    /// it inherits ([`Inheritable`]) or an object on the way to it, a number of a box,
    /// or a category of its resources. Such a page reads as if the object were
    /// `null`, as viewers draw it, so a copy of it would lose what the source draws.
    /// What else is read of the page tells the computation reading it itself, its
    /// content ([`Self::try_page_stream`]) and what its dictionary and resources
    /// refer to; what reading the page tree lost is the document's
    /// ([`Pages::unreadable`]).
    pub fn unreadable(&self) -> Option<ObjectIdentifier> {
        self.unreadable
    }

    /// Get the resources of the page.
    pub fn resources(&self) -> &Resources<'a> {
        &self.resources
    }

    /// Get the media box of the page.
    pub fn media_box(&self) -> Rect {
        self.media_box
    }

    /// Get the rotation of the page.
    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    /// Get the crop box of the page.
    pub fn crop_box(&self) -> Rect {
        self.crop_box
    }

    /// Return the intersection of crop box and media box.
    pub fn intersected_crop_box(&self) -> Rect {
        self.crop_box().intersect(self.media_box())
    }

    /// Return the base dimensions of the page (same as `intersected_crop_box`, but with special
    /// handling applied for zero-area pages).
    pub fn base_dimensions(&self) -> (f32, f32) {
        let crop_box = self.intersected_crop_box();

        if (crop_box.width() as f32).is_nearly_zero() || (crop_box.height() as f32).is_nearly_zero()
        {
            (A4.width() as f32, A4.height() as f32)
        } else {
            (
                crop_box.width().max(1.0) as f32,
                crop_box.height().max(1.0) as f32,
            )
        }
    }

    /// Return the with and height of the page that should be assumed when rendering the page.
    ///
    /// Depending on the document, it is either based on the media box or the crop box
    /// of the page. In addition to that, it also takes the rotation of the page into account.
    pub fn render_dimensions(&self) -> (f32, f32) {
        let (mut base_width, mut base_height) = self.base_dimensions();

        if matches!(
            self.rotation(),
            Rotation::Horizontal | Rotation::FlippedHorizontal
        ) {
            core::mem::swap(&mut base_width, &mut base_height);
        }

        (base_width, base_height)
    }

    /// Return an untyped iterator over the operators of the page's content stream.
    pub fn operations(&self) -> UntypedIter<'_> {
        self.operations_impl().unwrap_or(UntypedIter::empty())
    }

    /// Get the raw dictionary of the page.
    pub fn raw(&self) -> &Dict<'a> {
        &self.inner
    }

    /// Get the xref table (of the document the page belongs to).
    pub fn xref(&self) -> &'a XRef {
        self.ctx.xref()
    }

    /// Return a typed iterator over the operators of the page's content stream.
    pub fn typed_operations(&self) -> TypedIter<'_> {
        TypedIter::from_untyped(self.operations())
    }

    /// Return the initial transform that should be applied when rendering.
    ///
    /// This accounts for the mismatch between PDF's y-up and most renderers'
    /// y-down coordinate system, the rotation of the page and the offset of
    /// the crop box.
    pub fn initial_transform(&self, invert_y: bool) -> Transform {
        let crop_box = self.intersected_crop_box();
        let (_, base_height) = self.base_dimensions();
        let (width, height) = self.render_dimensions();

        let horizontal_t = Transform::ROTATE_CW_90 * Transform::translate((0.0, -width as f64));
        let flipped_horizontal_t =
            Transform::translate((0.0, height as f64)) * Transform::ROTATE_CCW_90;

        let rotation_transform = match self.rotation() {
            Rotation::None => Transform::IDENTITY,
            Rotation::Horizontal => {
                if invert_y {
                    horizontal_t
                } else {
                    flipped_horizontal_t
                }
            }
            Rotation::Flipped => {
                Transform::scale(-1.0) * Transform::translate((-width as f64, -height as f64))
            }
            Rotation::FlippedHorizontal => {
                if invert_y {
                    flipped_horizontal_t
                } else {
                    horizontal_t
                }
            }
        };

        let inversion_transform = if invert_y {
            Transform::new([1.0, 0.0, 0.0, -1.0, 0.0, base_height as f64])
        } else {
            Transform::IDENTITY
        };

        rotation_transform
            * inversion_transform
            * Transform::translate((-crop_box.x0, -crop_box.y0))
    }
}

/// A structure keeping track of the resources of a page.
#[derive(Clone, Debug)]
pub struct Resources<'a> {
    /// The raw dictionary of external graphics states.
    pub ext_g_states: Dict<'a>,
    /// The raw dictionary of fonts.
    pub fonts: Dict<'a>,
    /// The raw dictionary of properties.
    pub properties: Dict<'a>,
    /// The raw dictionary of color spaces.
    pub color_spaces: Dict<'a>,
    /// The raw dictionary of x objects.
    pub x_objects: Dict<'a>,
    /// The raw dictionary of patterns.
    pub patterns: Dict<'a>,
    /// The raw dictionary of shadings.
    pub shadings: Dict<'a>,
}

impl<'a> Resources<'a> {
    /// Create a new `Resources` object.
    pub fn new(resources: Dict<'a>) -> Self {
        Self::read(|key| resources.get::<Dict<'_>>(key))
    }

    /// The resources whose category for each key is `category`'s.
    fn read(mut category: impl FnMut(&'static [u8]) -> Option<Dict<'a>>) -> Self {
        let mut category = |key| category(key).unwrap_or_default();
        Self {
            ext_g_states: category(EXT_G_STATE),
            fonts: category(FONT),
            color_spaces: category(COLORSPACE),
            x_objects: category(XOBJECT),
            patterns: category(PATTERN),
            shadings: category(SHADING),
            properties: category(PROPERTIES),
        }
    }

    /// Get an external graphics state by name.
    pub fn get_ext_g_state(&self, name: &Name<'_>) -> Option<Dict<'a>> {
        self.ext_g_states.get(name)
    }

    /// Get a color space by name.
    pub fn get_color_space(&self, name: &Name<'_>) -> Option<Object<'a>> {
        self.color_spaces.get(name)
    }

    /// Get a font by name.
    pub fn get_font(&self, name: &Name<'_>) -> Option<Dict<'a>> {
        self.fonts.get(name)
    }

    /// Get a pattern by name.
    pub fn get_pattern(&self, name: &Name<'_>) -> Option<Object<'a>> {
        self.patterns.get(name)
    }

    /// Get an x object by name.
    pub fn get_x_object(&self, name: &Name<'_>) -> Option<Stream<'a>> {
        self.x_objects.get(name)
    }

    /// Get a shading by name.
    pub fn get_shading(&self, name: &Name<'_>) -> Option<Object<'a>> {
        self.shadings.get(name)
    }
}

// <https://github.com/apache/pdfbox/blob/a53a70db16ea3133994120bcf1e216b9e760c05b/pdfbox/src/main/java/org/apache/pdfbox/pdmodel/common/PDRectangle.java#L38>
const POINTS_PER_INCH: f64 = 72.0;
const POINTS_PER_MM: f64 = 1.0 / (10.0 * 2.54) * POINTS_PER_INCH;

/// The dimension of an A4 page.
pub const A4: Rect = Rect {
    x0: 0.0,
    y0: 0.0,
    x1: 210.0 * POINTS_PER_MM,
    y1: 297.0 * POINTS_PER_MM,
};

pub(crate) mod cached {
    use crate::page::Pages;
    use crate::reader::ReaderContext;
    use crate::xref::XRef;
    use core::ops::Deref;

    // Keep in sync with the implementation in `sync`. We duplicate it here
    // to make it more visible since we have unsafe code here.
    #[cfg(feature = "std")]
    pub(crate) use std::sync::Arc;

    #[cfg(not(feature = "std"))]
    pub(crate) use alloc::rc::Rc as Arc;

    pub(crate) struct CachedPages {
        pages: Pages<'static>,
        // NOTE: `pages` references the data in `xref`, so it's important that `xref`
        // appears after `pages` in the struct definition to ensure correct drop order.
        _xref: Arc<XRef>,
    }

    impl CachedPages {
        pub(crate) fn new(xref: Arc<XRef>) -> Option<Self> {
            // SAFETY:
            // - The XRef's location is stable in memory:
            //   - We wrapped it in a `Arc` (or `Rc` in `no_std`), which implements `StableDeref`.
            //   - The struct owns the `Arc`, ensuring that the inner value is not dropped during the whole
            //     duration.
            // - The internal 'static lifetime is not leaked because its rewritten
            //   to the self-lifetime in `pages()`.
            let xref_reference: &'static XRef = unsafe { core::mem::transmute(xref.deref()) };

            let ctx = ReaderContext::new(xref_reference, false);
            let pages = Pages::new(&ctx, xref_reference)?;

            Some(Self { pages, _xref: xref })
        }

        pub(crate) fn get(&self) -> &Pages<'_> {
            &self.pages
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{A4, Inheritable, Inheritance};
    use crate::object::ObjectIdentifier;
    use crate::pdf::Pdf;
    use alloc::string::String;
    use alloc::vec::Vec;

    const CATALOG: &str = "<< /Type /Catalog /Pages 2 0 R >>";

    /// A damaged file without a cross-reference table, whose objects, each a number,
    /// a generation and a body, are found by scanning it.
    fn recovered(objects: &[(i32, i32, &str)]) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        for (number, generation, body) in objects {
            out.extend(format!("{number} {generation} obj\n{body}\nendobj\n").bytes());
        }
        out.extend(b"trailer\n<< /Root 1 0 R >>\n%%EOF\n");
        out
    }

    /// The entries and bytes of the dictionaries parsed while `file` loads and an
    /// [`Inheritance`] of its own looks up every entry of every page, as a reader of
    /// the pages does, once it is checked that each page has the `fonts` fonts.
    fn parsed(file: Vec<u8>, fonts: usize) -> (usize, usize) {
        use crate::object::dict::PARSED;
        PARSED.set((0, 0));
        let pdf = Pdf::new(file).unwrap();
        let mut tree = Inheritance::new(&pdf);
        for page in pdf.pages().iter() {
            for key in Inheritable::ALL {
                let _ = tree.get(page, key);
            }
        }
        let parsed = PARSED.get();
        for page in pdf.pages().iter() {
            assert_eq!(page.resources().fonts.len(), fonts);
        }
        parsed
    }

    // A damaged file whose `n` pages are in one object stream, which the scan that
    // rebuilds its table reads, uncompressed, past each page: the stream is read
    // once, and no page is read as a stream, which searches the rest of the file.
    #[test]
    fn pages_in_one_object_stream_load_in_linear_time() {
        let work = |n: usize| {
            let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 3)).collect();
            let (mut header, mut data) = (String::new(), String::new());
            for i in 0..n {
                header += &format!("{} {} ", i + 3, data.len());
                data += "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>\n";
            }
            let stream = format!(
                "<< /Type /ObjStm /N {n} /First {} /Length {} >>\nstream\n{header}{data}\nendstream",
                header.len(),
                header.len() + data.len()
            );
            let tree = format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" "));
            let file = recovered(&[(1, 0, CATALOG), (2, 0, &tree), (n as i32 + 3, 0, &stream)]);
            crate::work::OBJECT_STREAM_ENTRIES.set(0);
            crate::object::stream::SEARCHED.set(0);
            let pdf = Pdf::new(file).unwrap();
            assert_eq!(pdf.pages().len(), n);
            assert!(pdf.pages().iter().all(|page| page.media_box().x1 == 200.0));
            (
                crate::work::OBJECT_STREAM_ENTRIES.get(),
                crate::object::stream::SEARCHED.get(),
            )
        };
        let [small, large] = [500, 1000].map(work);
        assert!(
            large.0 < small.0 * 21 / 10 && large.1 < small.1 * 21 / 10 + 100,
            "(entries, bytes searched) {small:?} for 500 pages, {large:?} for 1000"
        );
    }

    // A damaged object stream whose header lists `n` numbers at one offset holds one
    // object, which the entry of each number is, each in its own context: the `n`
    // pages they are parse it once, so loading them parses bytes linear in the file.
    #[test]
    fn entries_at_one_offset_of_an_object_stream_parse_it_once() {
        let parsed = |n: usize| {
            let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 3)).collect();
            let header: String = (0..n).map(|i| format!("{} 0 ", i + 3)).collect();
            let keys: String = (0..n).map(|i| format!("/K{i} 0 ")).collect();
            let page = format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] {keys}>>");
            let stream = format!(
                "<< /Type /ObjStm /N {n} /First {} /Length {} >>\nstream\n{header}{page}\nendstream",
                header.len(),
                header.len() + page.len()
            );
            let tree = format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" "));
            let file = recovered(&[(1, 0, CATALOG), (2, 0, &tree), (n as i32 + 3, 0, &stream)]);
            let pdf = Pdf::new(file.clone()).unwrap();
            let pages = pdf.pages();
            assert_eq!(pages.len(), n);
            for (i, page) in pages.iter().enumerate() {
                assert_eq!(page.media_box().x1, 200.0);
                assert_eq!(page.obj_id(), Some(ObjectIdentifier::new(i as i32 + 3, 0)));
            }
            parsed(file, 0)
        };
        let [small, large] = [250, 500].map(parsed);
        assert!(
            large.0 < small.0 * 22 / 10 && large.1 < small.1 * 22 / 10,
            "(entries, bytes) parsed {small:?} for 250 entries, {large:?} for 500"
        );
    }

    /// A file whose body holds `objects` in order, each a number and a body, a number
    /// possibly twice, and whose table lists each number at its first definition; at
    /// `objects[at]` for `(number, at)` in `misplaced`; and not at all for a number
    /// in `unlisted`.
    fn damaged(objects: &[(i32, &str)], misplaced: &[(i32, usize)], unlisted: &[i32]) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut first = std::collections::BTreeMap::new();
        let mut offsets = vec![];
        for (number, body) in objects {
            first.entry(*number).or_insert(out.len());
            offsets.push(out.len());
            out.extend(format!("{number} 0 obj\n{body}\nendobj\n").bytes());
        }
        let listed = misplaced.iter().map(|(number, _)| number);
        let size = first.keys().chain(listed).max().unwrap() + 1;
        let xref = out.len();
        out.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").bytes());
        for number in 1..size {
            let offset = match misplaced.iter().find(|(n, _)| *n == number) {
                Some((_, at)) => Some(offsets[*at]),
                None => first.get(&number).copied(),
            };
            match offset.filter(|_| !unlisted.contains(&number)) {
                Some(offset) => out.extend(format!("{offset:010} 00000 n \n").bytes()),
                None => out.extend(b"0000000000 65535 f \n"),
            }
        }
        out.extend(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes(),
        );
        out
    }

    // A resource category that refers to a dictionary read before — the page, the node
    // holding it, or the resources dictionary itself — is that dictionary, as
    // `PyMuPDF` 1.28.2 draws the form /F such a category names.
    #[test]
    fn a_category_that_refers_to_the_dictionary_holding_it_is_that_dictionary() {
        let page = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200]";
        for (node, page) in [
            (
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                &*format!("{page} /Resources << /XObject 3 0 R >> /F 4 0 R >>"),
            ),
            (
                "<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources << /XObject 2 0 R >> \
                 /F 4 0 R >>",
                &*format!("{page} >>"),
            ),
            (
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                &*format!("{page} /Resources 5 0 R >>"),
            ),
        ] {
            let file = damaged(
                &[
                    (1, CATALOG),
                    (2, node),
                    (3, page),
                    (4, &drawing()),
                    (5, "<< /XObject 5 0 R /F 4 0 R >>"),
                ],
                &[],
                &[],
            );
            let pdf = Pdf::new(file).unwrap();
            let page = &pdf.pages()[0];
            assert!(page.resources().x_objects.contains_key(b"F"), "{node}");
            assert_eq!(page.unreadable(), None);
        }
    }

    // Every inherited entry that refers to the node holding it is that node, read
    // whole: a dictionary, which is no box and no rotation.
    #[test]
    fn every_inherited_entry_that_refers_to_its_own_node_is_that_node() {
        let file = damaged(
            &[
                (1, CATALOG),
                (
                    2,
                    "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox 2 0 R /CropBox 2 0 R \
                     /Rotate 2 0 R /Resources 2 0 R >>",
                ),
                (3, "<< /Type /Page /Parent 2 0 R >>"),
            ],
            &[],
            &[],
        );
        let pdf = Pdf::new(file).unwrap();
        let page = &pdf.pages()[0];
        let mut lookups = Inheritance::new(&pdf);
        for key in Inheritable::ALL {
            let found = lookups.get(page, key);
            assert!(
                matches!(found, Ok(Some(crate::object::Object::Dict(_)))),
                "{key:?}: {found:?}"
            );
        }
        assert_eq!(page.media_box().x1, A4.x1);
        assert_eq!(page.unreadable(), None);
    }

    const DRAWING: &[u8] = b"0 0 1 rg 10 10 80 80 re f";

    fn drawing() -> String {
        let data = core::str::from_utf8(DRAWING).unwrap();
        format!("<< /Length {} >>\nstream\n{data}\nendstream", DRAWING.len())
    }

    mod content {
        use crate::crypto::fixture::{Part, aes128};
        use crate::object::ObjectIdentifier;
        use crate::object::stream::DecodeFailure;
        use crate::page::ContentError;
        use crate::pdf::Pdf;
        use alloc::vec::Vec;

        const RED: &[u8] = b"1 0 0 rg 0 0 10 10 re f";
        const BLUE: &[u8] = b"0 0 1 rg 10 0 10 10 re f";

        const CATALOG: Part<'_> = Part::Plain("<< /Type /Catalog /Pages 2 0 R >>");
        const TREE: Part<'_> = Part::Plain("<< /Type /Pages /Kids [3 0 R] /Count 1 >>");

        fn page(contents: &str) -> String {
            format!("<< /Type /Page /MediaBox [0 0 100 100] /Contents {contents} >>")
        }

        /// The page's content read strictly and leniently.
        fn read(file: Vec<u8>) -> (Result<Vec<u8>, ContentError>, Vec<u8>) {
            let pdf = Pdf::new(file).unwrap();
            let page = &pdf.pages()[0];
            let lenient = page.page_stream().unwrap_or_default().to_vec();
            (page.try_page_stream(), lenient)
        }

        fn joined(parts: &[&[u8]]) -> Vec<u8> {
            parts
                .iter()
                .flat_map(|p| p.iter().chain(b" "))
                .copied()
                .collect()
        }

        #[test]
        fn a_part_that_cannot_be_read_is_an_error() {
            let id = |n| Some(ObjectIdentifier::new(n, 0));
            let page = page("[4 0 R 5 0 R]");
            let file = |part: Part<'_>| {
                aes128(&[
                    CATALOG,
                    TREE,
                    Part::Plain(&page),
                    Part::Stream {
                        dict: "",
                        data: RED,
                        encrypt: true,
                    },
                    part,
                ])
            };
            // Data that is no Flate stream.
            let (strict, lenient) = read(file(Part::Stream {
                dict: "/Filter /FlateDecode",
                data: b"not flate at all, not flate at all",
                encrypt: true,
            }));
            assert!(
                matches!(strict, Err(ContentError::Decode(i, DecodeFailure::StreamDecode)) if i == id(5))
            );
            // A lenient reading recovers nothing of it.
            assert_eq!(lenient, joined(&[RED, b""]));
            // Data that does not decrypt: AES needs whole blocks.
            let (strict, _) = read(file(Part::Stream {
                dict: "",
                data: BLUE,
                encrypt: false,
            }));
            assert!(
                matches!(strict, Err(ContentError::Decode(i, DecodeFailure::Decryption)) if i == id(5))
            );
            // An object that is no stream.
            let (strict, _) = read(file(Part::Plain("<< /Length 3 >>")));
            assert!(matches!(strict, Err(ContentError::Unreadable(i)) if i == id(5)));
        }

        #[test]
        fn no_contents_is_an_empty_page() {
            // A missing /Contents, a null one, a null part and a part the file lacks.
            for contents in ["null", "[null 9 0 R]", "9 0 R"] {
                let page = page(contents);
                let file = aes128(&[CATALOG, TREE, Part::Plain(&page)]);
                assert_eq!(read(file).0.unwrap(), b"", "{contents}");
            }
            let bare = "<< /Type /Page /MediaBox [0 0 100 100] >>";
            let file = aes128(&[CATALOG, TREE, Part::Plain(bare)]);
            assert_eq!(read(file).0.unwrap(), b"");
            // An empty stream, which has no IV to decrypt with.
            let page = page("4 0 R");
            let empty = Part::Stream {
                dict: "",
                data: b"",
                encrypt: false,
            };
            let file = aes128(&[CATALOG, TREE, Part::Plain(&page), empty]);
            assert_eq!(read(file).0.unwrap(), b"");
        }
    }
}
