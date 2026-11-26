use crate::object::ObjectIdentifier;

use crate::hash::Map;
use crate::reader::ReaderContext;
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

// Gives decoded object streams the same lifetime as the original data.
pub(crate) struct Data {
    pub(crate) watched: crate::lost::Watched,
    pub(crate) dictionaries: Mutex<Map<usize, (usize, crate::object::dict::Index)>>,
    pub(crate) numbers: Mutex<Map<(usize, usize), (usize, crate::object::Number)>>,
    pub(crate) arrays: Mutex<Map<usize, usize>>,
    pub(crate) rectangles: Mutex<Map<usize, [crate::object::MaybeRef<f32>; 4]>>,
    data: PdfData,
    // 32 segments are more than enough as we can't have more objects than this.
    object_streams: SegmentList<ObjectStream, 32>,
    map: Mutex<Map<ObjectIdentifier, (usize, crate::lost::Lost)>>,
}

impl Debug for Data {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "Data {{ ... }}")
    }
}

impl Data {
    pub(crate) fn new(data: PdfData) -> Self {
        Self {
            data,
            dictionaries: Mutex::default(),
            numbers: Mutex::default(),
            arrays: Mutex::default(),
            rectangles: Mutex::default(),
            watched: crate::lost::Watched::default(),
            object_streams: SegmentList::new(),
            map: Mutex::new(Map::default()),
        }
    }

    pub(crate) fn get(&self) -> &PdfData {
        &self.data
    }

    pub(crate) fn object_stream(
        &self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'_>,
    ) -> Option<&ObjectStream> {
        if let Some(&(idx, lost)) = self.map.get().get(&id) {
            ctx.xref().note(lost);
            return self.object_streams.get(idx);
        }

        // Decoding may resolve other objects, so do it without holding the cache lock.
        let (object_stream, lost) = ctx.xref().apart(|| {
            let crate::xref::Carrier(stream, cut) = ctx.xref().get_with(id, ctx)?;
            let mut object_stream = ObjectStream::new(&stream)?;
            object_stream.next_byte = cut;
            Some(object_stream)
        });
        let object_stream = object_stream?;
        let mut locked = self.map.get();
        let next = locked.len();
        let (idx, lost) = *locked.entry(id).or_insert_with(|| {
            self.object_streams.get_or_init(next, || object_stream);
            (next, lost)
        });
        ctx.xref().note(lost);
        self.object_streams.get(idx)
    }
}
