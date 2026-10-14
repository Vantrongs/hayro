//! Reading bytes and PDF objects from data.

use crate::object::ObjectIdentifier;
use crate::sync::Arc;
use crate::trivia::{Comment, is_eol_character, is_white_space_character};
use crate::xref::XRef;
use smallvec::{SmallVec, smallvec};

pub use crate::byte_reader::Reader;

/// Extension trait for the `Reader` struct.
pub trait ReaderExt<'a> {
    fn read<T: Readable<'a>>(&mut self, ctx: &ReaderContext<'a>) -> Option<T>;
    fn read_with_context<T: Readable<'a>>(&mut self, ctx: &ReaderContext<'a>) -> Option<T>;
    fn read_without_context<T: Readable<'a>>(&mut self) -> Option<T>;
    fn skip<T: Skippable>(&mut self, is_content_stream: bool) -> Option<&'a [u8]>;
    fn skip_with_depth<T: Skippable>(
        &mut self,
        is_content_stream: bool,
        depth: usize,
    ) -> Option<&'a [u8]>;
    fn skip_white_spaces(&mut self);
    fn read_white_space(&mut self) -> Option<()>;
    fn skip_eol_characters(&mut self);
    fn skip_white_spaces_and_comments(&mut self);
}

impl<'a> ReaderExt<'a> for Reader<'a> {
    // Note: If `PLAIN` is true, it means that the data we are about to read _might_ contain
    // an object reference instead of an actual object. if `PLAIN` is false, then an object
    // reference cannot occur. The main reason we make this distinction is that when parsing
    // a number, we cannot unambiguously distinguish whether it's a real number or the
    // start of an object reference. In content streams, object references cannot appear,
    // so in order to speed this up we set `PLAIN` to false, meaning that as soon as we
    // encounter a number we know it's a number, and don't need to do a look-ahead to ensure
    // that it's not an object reference.
    #[inline]
    fn read<T: Readable<'a>>(&mut self, ctx: &ReaderContext<'a>) -> Option<T> {
        let old_offset = self.offset;

        T::read(self, ctx).or_else(|| {
            self.offset = old_offset;

            None
        })
    }

    #[inline]
    fn read_with_context<T: Readable<'a>>(&mut self, ctx: &ReaderContext<'a>) -> Option<T> {
        self.read::<T>(ctx)
    }

    #[inline]
    fn read_without_context<T: Readable<'a>>(&mut self) -> Option<T> {
        self.read::<T>(ReaderContext::dummy_ref())
    }

    #[inline]
    fn skip<T: Skippable>(&mut self, is_content_stream: bool) -> Option<&'a [u8]> {
        self.skip_with_depth::<T>(is_content_stream, 0)
    }

    #[inline]
    fn skip_with_depth<T: Skippable>(
        &mut self,
        is_content_stream: bool,
        depth: usize,
    ) -> Option<&'a [u8]> {
        let old_offset = self.offset;

        T::skip_with_depth(self, is_content_stream, depth).or_else(|| {
            self.offset = old_offset;
            None
        })?;

        self.data.get(old_offset..self.offset)
    }

    #[inline]
    fn skip_white_spaces(&mut self) {
        while let Some(b) = self.peek_byte() {
            if is_white_space_character(b) {
                self.forward();
            } else {
                return;
            }
        }
    }

    #[inline]
    fn read_white_space(&mut self) -> Option<()> {
        if is_white_space_character(self.peek_byte()?) {
            let w = self.read_byte()?;

            if w == b'\r' && self.peek_byte().is_some_and(|b| b == b'\n') {
                self.read_byte()?;
            }

            return Some(());
        }

        None
    }

    #[inline]
    fn skip_eol_characters(&mut self) {
        while let Some(b) = self.peek_byte() {
            if is_eol_character(b) {
                self.forward();
            } else {
                return;
            }
        }
    }

    #[inline]
    fn skip_white_spaces_and_comments(&mut self) {
        while let Some(b) = self.peek_byte() {
            if is_white_space_character(b) {
                self.skip_white_spaces();
            } else if b == b'%' {
                Comment::skip(self, true);
            } else {
                return;
            }
        }
    }
}

#[derive(Clone, Debug)]
struct ReaderContextData<'a> {
    xref: &'a XRef,
    in_content_stream: bool,
    in_object_stream: bool,
    obj_number: Option<ObjectIdentifier>,
    parent_chain: SmallVec<[ObjectIdentifier; 8]>,
    /// While a value shared by every resolution chain is computed: the least index in
    /// the chain of an object a cycle check rejected (see [`ReaderContext::shared`]).
    rejected: Option<Arc<Rejected>>,
}

/// The least index in a resolution chain of an object a cycle check rejected.
#[derive(Debug)]
struct Rejected(
    #[cfg(feature = "std")] core::sync::atomic::AtomicUsize,
    #[cfg(not(feature = "std"))] core::cell::Cell<usize>,
);

impl Rejected {
    fn new() -> Self {
        Self(usize::MAX.into())
    }

    fn note(&self, index: usize) {
        #[cfg(feature = "std")]
        self.0
            .fetch_min(index, core::sync::atomic::Ordering::Relaxed);
        #[cfg(not(feature = "std"))]
        self.0.set(self.0.get().min(index));
    }

    fn least(&self) -> usize {
        #[cfg(feature = "std")]
        return self.0.load(core::sync::atomic::Ordering::Relaxed);
        #[cfg(not(feature = "std"))]
        return self.0.get();
    }
}

#[derive(Clone, Debug)]
enum ReaderContextInner<'a> {
    Shared(Arc<ReaderContextData<'a>>),
    Dummy { in_content_stream: bool },
}

/// Context for reading PDF objects.
#[derive(Clone, Debug)]
pub struct ReaderContext<'a>(ReaderContextInner<'a>);

impl<'a> ReaderContext<'a> {
    pub(crate) fn new(xref: &'a XRef, in_content_stream: bool) -> Self {
        Self(ReaderContextInner::Shared(Arc::new(ReaderContextData {
            xref,
            in_content_stream,
            obj_number: None,
            in_object_stream: false,
            parent_chain: smallvec![],
            rejected: None,
        })))
    }

    pub fn dummy() -> Self {
        Self(ReaderContextInner::Dummy {
            in_content_stream: false,
        })
    }

    pub(crate) fn dummy_ref() -> &'static Self {
        &Self(ReaderContextInner::Dummy {
            in_content_stream: true,
        })
    }

    #[inline]
    pub(crate) fn xref(&self) -> &'a XRef {
        match &self.0 {
            ReaderContextInner::Shared(inner) => inner.xref,
            ReaderContextInner::Dummy { .. } => XRef::dummy(),
        }
    }

    #[inline]
    pub(crate) fn in_content_stream(&self) -> bool {
        match &self.0 {
            ReaderContextInner::Shared(inner) => inner.in_content_stream,
            ReaderContextInner::Dummy { in_content_stream } => *in_content_stream,
        }
    }

    #[inline]
    pub(crate) fn in_object_stream(&self) -> bool {
        match &self.0 {
            ReaderContextInner::Shared(inner) => inner.in_object_stream,
            ReaderContextInner::Dummy { .. } => false,
        }
    }

    #[inline]
    pub(crate) fn obj_number(&self) -> Option<ObjectIdentifier> {
        match &self.0 {
            ReaderContextInner::Shared(inner) => inner.obj_number,
            ReaderContextInner::Dummy { .. } => None,
        }
    }

    /// The entry whose key decrypts the strings and streams read in this context;
    /// `None` where they are not decrypted. What is read depends on it, so a cache
    /// of what is read keys by it ([`crate::xref::ContentKey`]).
    #[inline]
    pub(crate) fn decryption(&self) -> Option<ObjectIdentifier> {
        if self.xref().needs_decryption(self) {
            self.obj_number()
        } else {
            None
        }
    }

    #[inline]
    pub(crate) fn set_obj_number(&mut self, id: ObjectIdentifier) {
        match &mut self.0 {
            ReaderContextInner::Shared(inner) => Arc::make_mut(inner).obj_number = Some(id),
            ReaderContextInner::Dummy { .. } => {}
        }
    }

    #[inline]
    pub(crate) fn set_in_content_stream(&mut self, val: bool) {
        match &mut self.0 {
            ReaderContextInner::Shared(inner) => Arc::make_mut(inner).in_content_stream = val,
            ReaderContextInner::Dummy { in_content_stream } => *in_content_stream = val,
        }
    }

    #[inline]
    pub(crate) fn set_in_object_stream(&mut self, val: bool) {
        match &mut self.0 {
            ReaderContextInner::Shared(inner) => Arc::make_mut(inner).in_object_stream = val,
            ReaderContextInner::Dummy { .. } => {}
        }
    }

    /// Whether `id` is being resolved already in this context's chain, which a cycle
    /// check rejects: a value computed for [`Self::shared`] notes where.
    #[inline]
    pub(crate) fn rejects(&self, id: &ObjectIdentifier) -> bool {
        match &self.0 {
            ReaderContextInner::Shared(inner) => {
                let Some(index) = inner.parent_chain.iter().position(|other| other == id) else {
                    return false;
                };
                if let Some(rejected) = &inner.rejected {
                    rejected.note(index);
                }
                true
            }
            ReaderContextInner::Dummy { .. } => false,
        }
    }

    /// The value `compute` finds for an object `id` it resolves first, from this
    /// context, and whether every resolution chain finds it, so that it may be kept
    /// for all.
    ///
    /// A cycle check rejects an object already being resolved in the chain, so what
    /// is computed from `id` depends on the chain where a check rejected an object
    /// the chain held before `id` would be resolved, `id` itself included where the
    /// chain holds it already; a check rejecting `id` or an object resolved from it
    /// after that rejects it from any chain.
    pub(crate) fn shared<T>(&self, compute: impl FnOnce(&Self) -> T) -> (T, bool) {
        let ReaderContextInner::Shared(inner) = &self.0 else {
            return (compute(self), true);
        };
        // The index `id` is resolved at.
        let own = inner.parent_chain.len();
        let rejected = Arc::new(Rejected::new());
        let mut ctx = self.clone();
        Arc::make_mut(match &mut ctx.0 {
            ReaderContextInner::Shared(inner) => inner,
            ReaderContextInner::Dummy { .. } => unreachable!(),
        })
        .rejected = Some(rejected.clone());
        let value = compute(&ctx);
        let least = rejected.least();
        // What depends on this chain depends on it for the value being computed
        // around this one, if any.
        if least != usize::MAX
            && let Some(around) = &inner.rejected
        {
            around.note(least);
        }
        (value, least >= own)
    }

    #[inline]
    pub(crate) fn parent_chain_push(&mut self, id: ObjectIdentifier) {
        match &mut self.0 {
            ReaderContextInner::Shared(inner) => Arc::make_mut(inner).parent_chain.push(id),
            ReaderContextInner::Dummy { .. } => {}
        }
    }
}

pub trait Readable<'a>: Sized {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self>;
    fn from_bytes_impl(b: &'a [u8]) -> Option<Self> {
        let mut r = Reader::new(b);
        Self::read(&mut r, &ReaderContext::dummy())
    }
}

pub trait Skippable {
    fn skip(r: &mut Reader<'_>, is_content_stream: bool) -> Option<()>;

    fn skip_with_depth(r: &mut Reader<'_>, is_content_stream: bool, _: usize) -> Option<()> {
        Self::skip(r, is_content_stream)
    }
}
