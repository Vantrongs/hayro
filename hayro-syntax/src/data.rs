use crate::lost::{Lost, Watched};
use crate::object::ObjectIdentifier;
use crate::object::Stream;
use crate::reader::ReaderContext;
use crate::sync::FxHashMap;
use crate::sync::{Arc, Mutex, MutexExt};
use crate::util::SegmentList;
use crate::xref::ObjectStream;
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
    map: Mutex<FxHashMap<ObjectIdentifier, usize>>,
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
            map: Mutex::new(FxHashMap::default()),
            watched: Watched::default(),
        }
    }

    /// Get access to the original data of the PDF.
    pub(crate) fn get(&self) -> &PdfData {
        &self.data
    }

    /// The object stream `id`, read once; what reading it lost is told to every
    /// computation that asks for it ([`crate::lost`]). Where it cannot be read as an
    /// object stream, the objects the table puts in it are lost, and so is it.
    pub(crate) fn get_with(
        &self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'_>,
    ) -> Option<&ObjectStream> {
        let kept = |idx: usize| {
            let (stream, lost) = self.decoded.get(idx)?;
            self.watched.note(*lost);
            stream.as_ref()
        };
        if let Some(&idx) = self.map.get().get(&id) {
            return kept(idx);
        }

        // Decoding may resolve other objects, so do it without holding the cache lock.
        // A stream that is read differently from another resolution chain, such as
        // one rejected while it is being read already, is not kept, nor returned:
        // what is read from it borrows from what is kept.
        let ((stream, lost), shared) = ctx.shared(|ctx| {
            self.watched.watch(true, || {
                let objects = ctx
                    .xref()
                    .get_with::<Stream<'_>>(id, ctx)
                    .and_then(|stream| ObjectStream::new(&stream));
                // Unless the chain being read rejects it, as a cycle.
                if objects.is_none() && !ctx.rejects(&id) {
                    self.watched.note(Some(id));
                }
                objects
            })
        });
        if !shared {
            return None;
        }

        let mut locked = self.map.get();
        if let Some(&idx) = locked.get(&id) {
            return kept(idx);
        }

        let idx = locked.len();
        let (stream, _) = self.decoded.get_or_init(idx, || (stream, lost));
        locked.insert(id, idx);

        stream.as_ref()
    }
}
