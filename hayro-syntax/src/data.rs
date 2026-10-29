use crate::hash;
use crate::lost::{Lost, Watched};
use crate::object::ObjectIdentifier;
use crate::reader::ReaderContext;
use crate::sync::{Arc, Mutex, MutexExt};
use crate::util::SegmentList;
use crate::xref::{ObjectStream, Place};
use alloc::vec::Vec;
use core::fmt::{Debug, Formatter};

/// A container for the bytes of a PDF file.
#[derive(Clone)]
pub struct PdfData {
    #[cfg(feature = "std")]
    inner: Arc<dyn AsRef<[u8]> + Send + Sync>,
    #[cfg(not(feature = "std"))]
    inner: Arc<dyn AsRef<[u8]>>,
}

impl Debug for PdfData {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "PdfData {{ ... }}")
    }
}

impl AsRef<[u8]> for PdfData {
    fn as_ref(&self) -> &[u8] {
        (*self.inner).as_ref()
    }
}

#[cfg(feature = "std")]
impl<T: AsRef<[u8]> + Send + Sync + 'static> From<Arc<T>> for PdfData {
    fn from(data: Arc<T>) -> Self {
        Self { inner: data }
    }
}

#[cfg(not(feature = "std"))]
impl<T: AsRef<[u8]> + 'static> From<Arc<T>> for PdfData {
    fn from(data: Arc<T>) -> Self {
        Self { inner: data }
    }
}

impl From<Vec<u8>> for PdfData {
    fn from(data: Vec<u8>) -> Self {
        Self {
            inner: Arc::new(data),
        }
    }
}

/// A structure for storing the data of the PDF.
// To explain further: This crate uses a zero-parse approach, meaning that objects like
// dictionaries or arrays always store the underlying data and parse objects lazily as needed,
// instead of allocating the data and storing it in an owned way. However, the problem is that
// not all data is readily available in the original data of the PDF: Objects can also be
// stored in an object streams, in which case we first need to decode the stream before we can
// access the data.
//
// The purpose of `Data` is to allow us to access the original data as well as maybe decoded data
// by faking the same lifetime, so that we don't run into lifetime issues when dealing with
// PDF objects that actually stem from different data sources.
pub(crate) struct Data {
    data: PdfData,
    // 32 segments are more than enough as we can't have more objects than this.
    // `None` for a stream that is no object stream, which is not read again; with
    // what reading it lost.
    decoded: SegmentList<(Option<ObjectStream>, Lost), 32>,
    /// Where the object stream at each place is kept.
    map: Mutex<hash::Map<Place, usize>>,
    /// The computations being watched ([`crate::xref::XRef::watch`]).
    pub(crate) watched: Watched,
}

impl Debug for Data {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "Data {{ ... }}")
    }
}

impl Data {
    /// Create a new `Data` structure.
    pub(crate) fn new(data: PdfData) -> Self {
        Self {
            data,
            decoded: SegmentList::new(),
            map: Mutex::new(hash::Map::default()),
            watched: Watched::default(),
        }
    }

    /// How many object streams are kept decoded.
    #[cfg(test)]
    pub(crate) fn kept_streams(&self) -> usize {
        self.map.get().len()
    }

    /// Get access to the original data of the PDF.
    pub(crate) fn get(&self) -> &PdfData {
        &self.data
    }

    /// The object stream `id` at `place`, decoded by `decode` from `ctx` once for
    /// every entry and every read leading to the place, and where it is kept; what
    /// reading it lost is told to every computation that asks for it
    /// ([`crate::lost`]). Where it cannot be read as an object stream, the objects
    /// the table puts in it are lost, and so is it.
    pub(crate) fn object_stream<'c>(
        &self,
        place: Place,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'c>,
        decode: impl FnOnce(&ReaderContext<'c>) -> Option<ObjectStream>,
    ) -> Decoded<'_> {
        let kept = |idx: usize| match self.decoded.get(idx) {
            Some((stream, lost)) => {
                self.watched.note(*lost);
                match stream {
                    Some(stream) => Decoded::Stream(idx, stream),
                    None => Decoded::Lost(lost.unwrap_or(id)),
                }
            }
            None => Decoded::Lost(id),
        };
        if let Some(&idx) = self.map.get().get(&place) {
            return kept(idx);
        }

        // Decoding may resolve other objects, so do it without holding the cache lock.
        // A stream that is read differently from another resolution chain, such as
        // one rejected while it is being read already, is not kept, nor returned:
        // what is read from it borrows from what is kept.
        let ((stream, lost), shared) = ctx.shared(|ctx| {
            self.watched.watch(true, || {
                let objects = decode(ctx);
                // Unless the chain being read rejects it, as a cycle.
                if objects.is_none() && !ctx.rejects(&place.link()) {
                    self.watched.note(Some(id));
                }
                objects
            })
        });
        if !shared {
            return Decoded::Rejected;
        }

        let mut locked = self.map.get();
        if let Some(&idx) = locked.get(&place) {
            drop(locked);
            return kept(idx);
        }

        let idx = locked.len();
        self.decoded.get_or_init(idx, || (stream, lost));
        locked.insert(place, idx);
        drop(locked);
        match self.decoded.get(idx) {
            Some((Some(stream), _)) => Decoded::Stream(idx, stream),
            _ => Decoded::Lost(lost.unwrap_or(id)),
        }
    }
}

/// An object stream as [`Data::object_stream`] finds it.
pub(crate) enum Decoded<'a> {
    /// Read, and kept at an index.
    Stream(usize, &'a ObjectStream),
    /// It cannot be read: what reading it lost, it if nothing else.
    Lost(ObjectIdentifier),
    /// The chain reading it rejects what reading it needs as a cycle, so it is
    /// neither read nor lost.
    Rejected,
}
