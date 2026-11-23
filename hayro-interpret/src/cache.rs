use crate::util::hash128;
use core::hash::Hash;
use hayro_syntax::hash;
use hayro_syntax::object::{Array, Dict, MaybeRef, Name, Null, ObjRef, Object, Stream};
use hayro_syntax::page::Resources;
use kurbo::{Affine, Rect};
use std::any::{Any, TypeId};
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex};

type CacheMap = hash::Map<u128, Option<Box<dyn Any + Send + Sync>>>;

/// The values interpreting a document keeps for all its pages, so that each is the
/// same whichever page, and whichever chain of references, read it first: a value
/// read from objects is built from the object it is kept for alone, read as where a
/// resolution starts ([`Self::rooted`]), and what its build keeps besides is made
/// from bytes alone ([`Building::of_bytes`]), or from a stream alone
/// ([`Building::of_stream`]).
#[derive(Clone)]
pub(crate) struct Cache(Arc<Mutex<CacheMap>>);

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Cache {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(hash::Map::default())))
    }

    /// The value `build` makes of `object`, kept for it: `build` is given the object
    /// read as where a resolution starts ([`Root::as_root`]) and nothing else read,
    /// so the value is what the object, which its key names, holds, not what the
    /// chain of references the first caller reached it by lets a resolution read
    /// (a reference back into that chain is refused as a cycle). Nor may `build`
    /// keep a value of its own in the cache that depends on how it was reached: it
    /// sees the cache as [`Building`].
    pub(crate) fn rooted<O: Root, T: Clone + Send + Sync + 'static>(
        &self,
        object: &O,
        build: fn(O, &Building<'_>) -> Option<T>,
    ) -> Option<T> {
        self.get_or_insert_with::<T>(object.cache_key(), || {
            build(object.as_root(), &Building(self))
        })
    }

    /// [`Self::rooted`], a value kept apart for each `with`, which `build` is given
    /// besides the object: what the object holds read for `with`.
    pub(crate) fn rooted_with<O: Root, W: Hash + Copy, T: Clone + Send + Sync + 'static>(
        &self,
        object: &O,
        with: W,
        build: fn(O, W, &Building<'_>) -> Option<T>,
    ) -> Option<T> {
        self.get_or_insert_with::<T>(hash128(&(object.cache_key(), with)), || {
            build(object.as_root(), with, &Building(self))
        })
    }

    /// [`Self::rooted_stream_in`] with [`Self::rooted_with`].
    fn rooted_stream_with_in<'a, W: Hash + Copy, T: Clone + Send + Sync + 'static>(
        &self,
        stream: &Stream<'a>,
        with: W,
        build: fn(Stream<'a>, W, &Building<'_>) -> Option<T>,
    ) -> Option<T> {
        match stream.identity() {
            Some(_) => self.rooted_with(stream, with, build),
            None => build(stream.clone(), with, &Building(self)),
        }
    }

    /// [`Self::rooted`] of `stream`, but for a stream that is no object of a loaded
    /// document ([`Stream::identity`], an inline image) built as it was reached and
    /// kept nowhere: what it is is its bytes, in the content each time it is drawn,
    /// which a key would hash each time, and which the cache would keep past the
    /// content.
    fn rooted_stream_in<'a, T: Clone + Send + Sync + 'static>(
        &self,
        stream: &Stream<'a>,
        build: fn(Stream<'a>, &Building<'_>) -> Option<T>,
    ) -> Option<T> {
        match stream.identity() {
            Some(_) => self.rooted(stream, build),
            None => build(stream.clone(), &Building(self)),
        }
    }

    pub(crate) fn get_or_insert_with<T: Clone + Send + Sync + 'static>(
        &self,
        key: u128,
        f: impl FnOnce() -> Option<T>,
    ) -> Option<T> {
        // Values of different types made of one object are kept apart.
        let id = hash128(&(TypeId::of::<T>(), key));
        let mut locked = self.0.lock().unwrap();

        // We can't use `get_or_insert_with` here, because if the closure makes another access to the
        // cache, we end up with a deadlock.
        match locked.entry(id) {
            Entry::Occupied(o) => o
                .get()
                .as_ref()
                .and_then(|val| val.downcast_ref::<T>().cloned()),
            Entry::Vacant(_) => {
                drop(locked);
                let val = f();
                self.0.lock().unwrap().insert(
                    id,
                    val.clone()
                        .map(|val| Box::new(val) as Box<dyn Any + Send + Sync>),
                );

                val
            }
        }
    }
}

/// The cache as a value built for it ([`Cache::rooted`]) sees it: it keeps what is
/// made of bytes alone, which no reference resolved while building decides.
pub(crate) struct Building<'c>(&'c Cache);

impl Building<'_> {
    /// The value `build` makes of `bytes` and `with`, kept for them: `build` is given
    /// them alone.
    pub(crate) fn of_bytes<W: Hash + Copy, T: Clone + Send + Sync + 'static>(
        &self,
        bytes: &[u8],
        with: W,
        build: fn(&[u8], W) -> Option<T>,
    ) -> Option<T> {
        self.0
            .get_or_insert_with::<T>(hash128(&(bytes, with)), || build(bytes, with))
    }

    /// [`Self::of_bytes`] of the decoded data of `stream`, kept for the stream's own
    /// object, which names it whatever chain of references reached it, so that its
    /// data are decoded once for the document whichever values are built on it:
    /// `None` where they do not decode. The stream is decoded as where a resolution
    /// starts ([`Stream::as_root`]).
    pub(crate) fn of_stream<W: Hash + Copy, T: Clone + Send + Sync + 'static>(
        &self,
        stream: &Stream<'_>,
        with: W,
        build: fn(&[u8], W) -> Option<T>,
    ) -> Option<Option<T>> {
        self.0
            .get_or_insert_with::<Option<T>>(hash128(&(stream.cache_key(), with)), || {
                let decoded = stream.as_root().decoded().ok()?;
                Some(self.of_bytes(&decoded, with, build))
            })
    }
}

/// Where a value built from a stream alone is kept ([`Cache::rooted`], for an inline
/// image nowhere): the document's cache, or the cache as a value built for it sees
/// it ([`Building`]), where the value, built from its own stream read as where a
/// resolution starts, depends on nothing the value being built was reached by.
pub(crate) trait KeepsStreams {
    /// The value `build` makes of `stream`, kept for it.
    fn rooted_stream<'a, T: Clone + Send + Sync + 'static>(
        &self,
        stream: &Stream<'a>,
        build: fn(Stream<'a>, &Building<'_>) -> Option<T>,
    ) -> Option<T>;

    /// The value `build` makes of `stream` for `with`, kept for both.
    fn rooted_stream_with<'a, W: Hash + Copy, T: Clone + Send + Sync + 'static>(
        &self,
        stream: &Stream<'a>,
        with: W,
        build: fn(Stream<'a>, W, &Building<'_>) -> Option<T>,
    ) -> Option<T>;
}

impl KeepsStreams for Cache {
    fn rooted_stream<'a, T: Clone + Send + Sync + 'static>(
        &self,
        stream: &Stream<'a>,
        build: fn(Stream<'a>, &Building<'_>) -> Option<T>,
    ) -> Option<T> {
        self.rooted_stream_in(stream, build)
    }

    fn rooted_stream_with<'a, W: Hash + Copy, T: Clone + Send + Sync + 'static>(
        &self,
        stream: &Stream<'a>,
        with: W,
        build: fn(Stream<'a>, W, &Building<'_>) -> Option<T>,
    ) -> Option<T> {
        self.rooted_stream_with_in(stream, with, build)
    }
}

impl KeepsStreams for Building<'_> {
    fn rooted_stream<'a, T: Clone + Send + Sync + 'static>(
        &self,
        stream: &Stream<'a>,
        build: fn(Stream<'a>, &Building<'_>) -> Option<T>,
    ) -> Option<T> {
        self.0.rooted_stream_in(stream, build)
    }

    fn rooted_stream_with<'a, W: Hash + Copy, T: Clone + Send + Sync + 'static>(
        &self,
        stream: &Stream<'a>,
        with: W,
        build: fn(Stream<'a>, W, &Building<'_>) -> Option<T>,
    ) -> Option<T> {
        self.0.rooted_stream_with_in(stream, with, build)
    }
}

/// An object a value is kept for by [`Cache::rooted`].
pub(crate) trait Root: CacheKey {
    /// The object read as where a resolution starts ([`Dict::as_root`]).
    fn as_root(&self) -> Self;
}

impl Root for Object<'_> {
    fn as_root(&self) -> Self {
        Object::as_root(self)
    }
}

impl Root for Stream<'_> {
    fn as_root(&self) -> Self {
        Stream::as_root(self)
    }
}

/// A trait for objects that can generate a unique cache key.
pub trait CacheKey {
    /// Returns the cache key for this object.
    fn cache_key(&self) -> u128;
}

impl<T: CacheKey, U: CacheKey> CacheKey for (T, U) {
    fn cache_key(&self) -> u128 {
        hash128(&(self.0.cache_key(), self.1.cache_key()))
    }
}

impl CacheKey for Dict<'_> {
    fn cache_key(&self) -> u128 {
        hash128(&(b"dict", self.content_key().value()))
    }
}

impl CacheKey for Stream<'_> {
    fn cache_key(&self) -> u128 {
        match self.identity() {
            Some(obj_id) => hash128(&obj_id),
            None => hash128(&(self.dict().data(), self.raw_data().as_ref())),
        }
    }
}

impl CacheKey for Resources<'_> {
    fn cache_key(&self) -> u128 {
        hash128(
            &[
                &self.ext_g_states,
                &self.fonts,
                &self.properties,
                &self.color_spaces,
                &self.x_objects,
                &self.patterns,
                &self.shadings,
            ]
            .map(Dict::cache_key),
        )
    }
}

impl CacheKey for Null {
    fn cache_key(&self) -> u128 {
        hash128(self)
    }
}

impl CacheKey for bool {
    fn cache_key(&self) -> u128 {
        hash128(self)
    }
}

impl CacheKey for hayro_syntax::object::Number {
    fn cache_key(&self) -> u128 {
        hash128(&self.as_f64().to_bits())
    }
}

impl CacheKey for hayro_syntax::object::String<'_> {
    fn cache_key(&self) -> u128 {
        hash128(self.as_ref())
    }
}

impl CacheKey for Name<'_> {
    fn cache_key(&self) -> u128 {
        hash128(self)
    }
}

impl CacheKey for Array<'_> {
    fn cache_key(&self) -> u128 {
        hash128(&(b"array", self.content_key().value()))
    }
}

impl CacheKey for Object<'_> {
    fn cache_key(&self) -> u128 {
        match self {
            Object::Null(n) => n.cache_key(),
            Object::Boolean(b) => b.cache_key(),
            Object::Number(n) => n.cache_key(),
            Object::String(s) => s.cache_key(),
            Object::Name(n) => n.cache_key(),
            Object::Dict(d) => d.cache_key(),
            Object::Array(a) => a.cache_key(),
            Object::Stream(s) => s.cache_key(),
        }
    }
}

impl CacheKey for ObjRef {
    fn cache_key(&self) -> u128 {
        hash128(self)
    }
}

impl<T: CacheKey> CacheKey for MaybeRef<T> {
    fn cache_key(&self) -> u128 {
        match self {
            Self::Ref(r) => r.cache_key(),
            Self::NotRef(o) => o.cache_key(),
        }
    }
}

impl CacheKey for Affine {
    fn cache_key(&self) -> u128 {
        let c = self.as_coeffs();
        hash128(&[
            c[0].to_bits(),
            c[1].to_bits(),
            c[2].to_bits(),
            c[3].to_bits(),
            c[4].to_bits(),
            c[5].to_bits(),
        ])
    }
}

impl CacheKey for Rect {
    fn cache_key(&self) -> u128 {
        hash128(&[
            self.x0.to_bits(),
            self.y0.to_bits(),
            self.x1.to_bits(),
            self.y1.to_bits(),
        ])
    }
}

impl CacheKey for u128 {
    fn cache_key(&self) -> u128 {
        hash128(self)
    }
}
