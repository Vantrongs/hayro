//! Arrays.

use crate::object::macros::object;
use crate::object::parsed::{Items, Node, Part};
use crate::object::r#ref::MaybeRef;
use crate::object::{FromBytes, Object, ObjectIdentifier, ObjectLike, Written};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::work::{Cancelled, Meter};
use crate::xref::ContentKey;
use alloc::vec::Vec;
use core::fmt::{Debug, Display, Formatter};
use core::marker::PhantomData;
use smallvec::SmallVec;

/// An array of PDF objects.
#[derive(Clone)]
pub struct Array<'a>(Repr<'a>);

#[derive(Clone)]
enum Repr<'a> {
    /// Items read from their bytes as they are iterated.
    Bytes {
        data: &'a [u8],
        ctx: ReaderContext<'a>,
    },
    /// Items of the body of an object, parsed once ([`crate::object::parsed`]).
    Parsed(Part<'a>),
}

// Note that this is not structural equality, i.e. two arrays with the same
// items are still considered different if they have different whitespaces.
impl PartialEq for Array<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.data() == other.data()
    }
}

impl<'a> Array<'a> {
    /// The array `part` reads.
    pub(crate) fn parsed(part: Part<'a>) -> Self {
        Self(Repr::Parsed(part))
    }

    /// Where the items are read from.
    fn cursor(&self) -> Cursor<'a> {
        match &self.0 {
            Repr::Bytes { data, .. } => Cursor::Bytes(Reader::new(data)),
            Repr::Parsed(part) => Cursor::Parsed {
                part: part.clone(),
                at: 0,
            },
        }
    }

    /// Returns an iterator over the objects of the array.
    pub fn raw_iter(&self) -> ArrayIter<'a> {
        ArrayIter {
            cursor: self.cursor(),
            ctx: self.ctx().clone(),
        }
    }

    /// Returns an iterator over the items of the array as the file writes them
    /// ([`Written`]).
    pub fn written(&self) -> impl Iterator<Item = Written<'a>> + use<'a> {
        let ctx = self.ctx().clone();
        let mut cursor = self.cursor();
        core::iter::from_fn(move || match &mut cursor {
            Cursor::Bytes(reader) => {
                let start = reader.offset();
                reader.skip_white_spaces_and_comments();
                if reader.at_end() {
                    return None;
                }
                // Objects are already guaranteed to be valid.
                let item = Written::read(reader, &ctx).unwrap();
                crate::work::lexed(reader.offset() - start);
                Some(item)
            }
            Cursor::Parsed { part, at } => {
                if *at >= part.len() {
                    return None;
                }
                *at += 1;
                Some(part.written(*at - 1))
            }
        })
    }

    /// Returns an iterator over the entries of the array that are objects of type
    /// `T`, references resolved. An entry of another type, or a reference to no
    /// object, is skipped: it does not end the iteration.
    #[allow(
        private_bounds,
        reason = "users shouldn't be able to implement `ObjectLike` for custom objects."
    )]
    pub fn iter<T>(&self) -> ResolvedArrayIter<'a, T>
    where
        T: ObjectLike<'a>,
    {
        ResolvedArrayIter {
            items: self.items(),
        }
    }

    /// Returns an iterator with one item for each entry of the array, in its place:
    /// the entry as an object of type `T`, references resolved, or `None` when it is
    /// of another type or a reference to no object.
    #[allow(
        private_bounds,
        reason = "users shouldn't be able to implement `ObjectLike` for custom objects."
    )]
    pub fn items<T>(&self) -> ArrayItems<'a, T>
    where
        T: ObjectLike<'a>,
    {
        const {
            assert!(
                T::ONE_ENTRY,
                "a tuple spans several entries; read it with `flex_iter`"
            );
        }
        ArrayItems {
            raw: self.raw_iter(),
            phantom_data: PhantomData,
        }
    }

    /// The items of the array that are of a specific type, resolved; `Err` with an
    /// object reading an item lost ([`crate::lost`]), the one it refers to where the
    /// file lists it but it cannot be read, which the computation being watched is
    /// told too. As in [`Self::iter`], an item of another type, or a reference to
    /// nothing, is passed over, not the end.
    #[allow(
        private_bounds,
        reason = "users shouldn't be able to implement `ObjectLike` for custom objects."
    )]
    pub fn iter_checked<T>(&self) -> impl Iterator<Item = Result<T, ObjectIdentifier>> + 'a
    where
        T: ObjectLike<'a>,
    {
        let ctx = self.ctx().clone();
        self.raw_iter().filter_map(move |item| match item {
            MaybeRef::Ref(r) => ctx.xref().get_checked_with(r.into(), &ctx).transpose(),
            MaybeRef::NotRef(object) => object.cast::<T>().map(Ok),
        })
    }

    /// The entries before the first one that is not a `T`, as `flex_iter` reads
    /// them; tuples read several entries at once.
    fn prefix<T: ObjectLike<'a>>(&self) -> impl Iterator<Item = T> + use<'a, T> {
        let mut entries = self.flex_iter();
        core::iter::from_fn(move || entries.next::<T>())
    }

    /// Return a flex iterator over the items in the array.
    pub fn flex_iter(&self) -> FlexArrayIter<'a> {
        FlexArrayIter {
            cursor: self.cursor(),
            ctx: self.ctx().clone(),
        }
    }

    /// The first `n` entries, and whether the one after them is a `T`, read as
    /// [`Self::items`] reads them and each counted on `meter` before it is: an array
    /// read for a few entries costs those, however many follow.
    #[allow(
        private_bounds,
        reason = "users shouldn't be able to implement `ObjectLike` for custom objects."
    )]
    pub fn leading<T>(&self, n: usize, meter: &Meter<'_>) -> Result<Leading<T>, Cancelled>
    where
        T: ObjectLike<'a>,
    {
        let mut items = self.items::<T>();
        let mut entries = SmallVec::new();
        let mut more = false;
        for i in 0..=n {
            meter.work(LEADING_ENTRY_WORK)?;
            crate::work::operation(1);
            let Some(entry) = items.next() else {
                break;
            };
            if i < n {
                entries.push(entry);
            } else {
                more = entry.is_some();
            }
        }
        Ok(Leading { entries, more, n })
    }

    /// Return the raw data of the array.
    pub fn data(&self) -> &'a [u8] {
        match &self.0 {
            Repr::Bytes { data, .. } => data,
            Repr::Parsed(part) => part.data(),
        }
    }

    /// What the array's items are read from, for a cache of what is read or copied
    /// from them.
    pub fn content_key(&self) -> ContentKey<'a> {
        ContentKey::new(self.data(), self.ctx())
    }

    /// The context the array's items are read in.
    pub(crate) fn ctx(&self) -> &ReaderContext<'a> {
        match &self.0 {
            Repr::Bytes { ctx, .. } => ctx,
            Repr::Parsed(part) => part.ctx(),
        }
    }
}

impl Debug for Array<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        let mut debug_list = f.debug_list();

        self.raw_iter().for_each(|i| {
            debug_list.entry(&i);
        });

        debug_list.finish()
    }
}

impl Display for Array<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.write_str("[")?;
        for (index, item) in self.raw_iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            Display::fmt(&item, f)?;
        }
        f.write_str("]")
    }
}

object!(Array<'a>, Array);

impl Skippable for Array<'_> {
    fn skip(r: &mut Reader<'_>, is_content_stream: bool) -> Option<()> {
        Self::skip_with_depth(r, is_content_stream, 0)
    }

    fn skip_with_depth(r: &mut Reader<'_>, is_content_stream: bool, depth: usize) -> Option<()> {
        if depth >= super::MAX_OBJECT_NESTING_DEPTH {
            return None;
        }

        parse_array_with(r, |r| {
            if is_content_stream {
                r.skip_with_depth::<Object<'_>>(true, depth + 1)?;
            } else {
                r.skip_with_depth::<MaybeRef<Object<'_>>>(false, depth + 1)?;
            }
            Some(())
        })
    }
}

/// Reads an array, `item` reading each item: the one way an array is read, whether
/// it is skipped or parsed.
pub(crate) fn parse_array_with<'a>(
    r: &mut Reader<'a>,
    mut item: impl FnMut(&mut Reader<'a>) -> Option<()>,
) -> Option<()> {
    r.forward_tag(b"[")?;

    loop {
        r.skip_white_spaces_and_comments();

        if let Some(()) = r.forward_tag(b"]") {
            return Some(());
        }
        item(r)?;
    }
}

impl Default for Array<'_> {
    fn default() -> Self {
        Self::from_bytes(b"[]").unwrap()
    }
}

impl<'a> Readable<'a> for Array<'a> {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let bytes = r.skip::<Array<'_>>(ctx.in_content_stream())?;
        crate::work::lexed(bytes.len());

        #[cfg(test)]
        super::dict::PARSED.with(|parsed| {
            let (entries, read) = parsed.get();
            parsed.set((entries, read + bytes.len()));
        });

        Some(Self(Repr::Bytes {
            data: &bytes[1..bytes.len() - 1],
            ctx: ctx.clone(),
        }))
    }
}

/// Where the next items of an array are read from.
enum Cursor<'a> {
    Bytes(Reader<'a>),
    Parsed { part: Part<'a>, at: usize },
}

/// An iterator over the items of an array.
pub struct ArrayIter<'a> {
    cursor: Cursor<'a>,
    ctx: ReaderContext<'a>,
}

impl<'a> Iterator for ArrayIter<'a> {
    type Item = MaybeRef<Object<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.cursor {
            Cursor::Bytes(reader) => {
                let start = reader.offset();
                reader.skip_white_spaces_and_comments();

                if reader.at_end() {
                    return None;
                }
                // The array was validated by skipping its items, which accepts some
                // items that can't be read. The read is returned as it is: unwrapping
                // and rewrapping it copies the object field by field, and the wider
                // reloads of those narrow stores stall store forwarding.
                let item = reader.read_with_context::<MaybeRef<Object<'_>>>(&self.ctx);
                if item.is_some() {
                    crate::work::lexed(reader.offset() - start);
                }
                item
            }
            Cursor::Parsed { part, at } => {
                if *at >= part.len() {
                    return None;
                }
                *at += 1;
                Some(part.item(*at - 1))
            }
        }
    }
}

/// An iterator over the entries of an array that are objects of a specific type
/// (see [`Array::iter`]).
pub struct ResolvedArrayIter<'a, T> {
    items: ArrayItems<'a, T>,
}

impl<'a, T> Iterator for ResolvedArrayIter<'a, T>
where
    T: ObjectLike<'a>,
{
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.items.next()? {
                return Some(item);
            }
        }
    }
}

/// An iterator over the entries of an array, each as an object of a specific type
/// or `None` (see [`Array::items`]).
pub struct ArrayItems<'a, T> {
    raw: ArrayIter<'a>,
    phantom_data: PhantomData<T>,
}

impl<'a, T> Iterator for ArrayItems<'a, T>
where
    T: ObjectLike<'a>,
{
    type Item = Option<T>;

    // Each entry read as an object and cast, as a parsed array's entries are, so
    // that an entry of another type is passed over by the one rule that read it.
    fn next(&mut self) -> Option<Self::Item> {
        Some(match self.raw.next()? {
            MaybeRef::Ref(r) => self.raw.ctx.xref().get_with::<T>(r.into(), &self.raw.ctx),
            MaybeRef::NotRef(object) => object.cast::<T>(),
        })
    }
}

/// The work an entry [`Array::leading`] reads counts as: the object read.
const LEADING_ENTRY_WORK: usize = size_of::<Object<'static>>();

/// The first entries of an array ([`Array::leading`]).
#[derive(Debug, Clone)]
pub struct Leading<T> {
    /// Each of the first `n` entries, or of all of a shorter array: a `T`, or `None`
    /// where it is of another type or a reference to no object.
    pub entries: SmallVec<[Option<T>; 8]>,
    /// Whether the entry after them is a `T`.
    pub more: bool,
    n: usize,
}

impl<T> Leading<T> {
    /// The `n` entries, where the array starts with `n` `T`s or more.
    pub fn at_least<C: FromIterator<T>>(self) -> Option<C> {
        if self.entries.len() < self.n {
            return None;
        }
        self.entries.into_iter().collect()
    }

    /// The `n` entries, where the array starts with `n` `T`s and no more: as reading
    /// it as a vector of `T`s reads `n` of them.
    pub fn exactly<C: FromIterator<T>>(self) -> Option<C> {
        if self.more {
            return None;
        }
        self.at_least()
    }
}

/// An iterator over the array that allows reading a different object each time.
pub struct FlexArrayIter<'a> {
    cursor: Cursor<'a>,
    ctx: ReaderContext<'a>,
}

impl<'a> FlexArrayIter<'a> {
    #[allow(
        private_bounds,
        reason = "users shouldn't be able to implement `ObjectLike` for custom objects."
    )]
    #[allow(clippy::should_implement_trait)]
    /// Try reading the next item as a specific object from the array.
    pub fn next<T: ObjectLike<'a>>(&mut self) -> Option<T> {
        match &mut self.cursor {
            Cursor::Bytes(reader) => {
                reader.skip_white_spaces_and_comments();

                if reader.at_end() {
                    return None;
                }
                let start = reader.offset();
                let item = reader.read_with_context::<MaybeRef<T>>(&self.ctx);
                crate::work::lexed(reader.offset() - start);
                match item? {
                    MaybeRef::Ref(r) => self.ctx.xref().get_with::<T>(r.into(), &self.ctx),
                    MaybeRef::NotRef(i) => Some(i),
                }
            }
            // As reading the item's bytes as a `MaybeRef<T>` reads them: a reference
            // is taken and resolved, else the item, or items for a tuple, as a `T`,
            // which are not taken where they are none.
            Cursor::Parsed { part, at } => {
                let mut items = Items { part, at: *at };
                let read = match part.items().get(items.at)? {
                    Node::Ref(_) => items.next_resolved::<T>(&self.ctx),
                    _ => Some(T::from_items(&mut items, &self.ctx)?),
                };
                *at = items.at;
                read
            }
        }
    }
}

impl<'a, T: ObjectLike<'a> + Copy + Default, const C: usize> TryFrom<Array<'a>> for [T; C] {
    type Error = ();

    fn try_from(value: Array<'a>) -> Result<Self, Self::Error> {
        let mut iter = value.prefix::<T>();

        let mut val = [T::default(); C];

        for i in 0..C {
            val[i] = iter.next().ok_or(())?;
        }

        if iter.next().is_some() {
            warn!("found excess elements in array");

            return Err(());
        }

        Ok(val)
    }
}

impl<'a, T: ObjectLike<'a> + Copy + Default, const C: usize> TryFrom<Object<'a>> for [T; C]
where
    [T; C]: TryFrom<Array<'a>, Error = ()>,
{
    type Error = ();

    fn try_from(value: Object<'a>) -> Result<Self, Self::Error> {
        match value {
            Object::Array(a) => a.try_into(),
            _ => Err(()),
        }
    }
}

impl<'a, T: ObjectLike<'a> + Copy + Default, const C: usize> Readable<'a> for [T; C] {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let array = Array::read(r, ctx)?;
        array.try_into().ok()
    }
}

impl<'a, T: ObjectLike<'a> + Copy + Default, const C: usize> ObjectLike<'a> for [T; C] {}

impl<'a, T: ObjectLike<'a>> TryFrom<Array<'a>> for Vec<T> {
    type Error = ();

    fn try_from(value: Array<'a>) -> Result<Self, Self::Error> {
        Ok(value.prefix::<T>().collect())
    }
}

impl<'a, T: ObjectLike<'a>> TryFrom<Object<'a>> for Vec<T> {
    type Error = ();

    fn try_from(value: Object<'a>) -> Result<Self, Self::Error> {
        match value {
            Object::Array(a) => a.try_into(),
            _ => Err(()),
        }
    }
}

impl<'a, T: ObjectLike<'a>> Readable<'a> for Vec<T> {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let array = Array::read(r, ctx)?;
        array.try_into().ok()
    }
}

impl<'a, T: ObjectLike<'a>> ObjectLike<'a> for Vec<T> {}

impl<'a, U: ObjectLike<'a>, T: ObjectLike<'a> + smallvec::Array<Item = U>> TryFrom<Array<'a>>
    for SmallVec<T>
{
    type Error = ();

    fn try_from(value: Array<'a>) -> Result<Self, Self::Error> {
        Ok(value.prefix::<U>().collect())
    }
}

impl<'a, U: ObjectLike<'a>, T: ObjectLike<'a> + smallvec::Array<Item = U>> TryFrom<Object<'a>>
    for SmallVec<T>
{
    type Error = ();

    fn try_from(value: Object<'a>) -> Result<Self, Self::Error> {
        match value {
            Object::Array(a) => a.try_into(),
            _ => Err(()),
        }
    }
}

impl<'a, U: ObjectLike<'a>, T: ObjectLike<'a> + smallvec::Array<Item = U>> Readable<'a>
    for SmallVec<T>
{
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let array = Array::read(r, ctx)?;
        array.try_into().ok()
    }
}

impl<'a, U: ObjectLike<'a>, T: ObjectLike<'a> + smallvec::Array<Item = U>> ObjectLike<'a>
    for SmallVec<T>
where
    U: Clone,
    U: Debug,
{
}

#[cfg(test)]
mod tests {
    use crate::object::Object;
    use crate::object::r#ref::MaybeRef;
    use crate::object::{Array, FromBytes};
    use crate::reader::Reader;
    use crate::reader::{ReaderContext, ReaderExt};
    use crate::xref::XRef;

    fn array_impl(data: &[u8]) -> Option<Vec<Object<'_>>> {
        Reader::new(data)
            .read_with_context::<Array<'_>>(&ReaderContext::new(XRef::dummy(), false))
            .map(|a| a.iter::<Object<'_>>().collect::<Vec<_>>())
    }

    fn array_ref_impl(data: &[u8]) -> Option<Vec<MaybeRef<Object<'_>>>> {
        Reader::new(data)
            .read_with_context::<Array<'_>>(&ReaderContext::new(XRef::dummy(), false))
            .map(|a| a.raw_iter().collect::<Vec<_>>())
    }

    #[test]
    fn empty_array_1() {
        let res = array_impl(b"[]").unwrap();
        assert!(res.is_empty());
    }

    #[test]
    fn empty_array_2() {
        let res = array_impl(b"[   \n]").unwrap();
        assert!(res.is_empty());
    }

    #[test]
    fn array_1() {
        let res = array_impl(b"[34]").unwrap();
        assert!(matches!(res[0], Object::Number(_)));
    }

    #[test]
    fn array_2() {
        let res = array_impl(b"[true  ]").unwrap();
        assert!(matches!(res[0], Object::Boolean(_)));
    }

    #[test]
    fn array_3() {
        let res = array_impl(b"[true \n false 34.564]").unwrap();
        assert!(matches!(res[0], Object::Boolean(_)));
        assert!(matches!(res[1], Object::Boolean(_)));
        assert!(matches!(res[2], Object::Number(_)));
    }

    #[test]
    fn array_4() {
        let res = array_impl(b"[(A string.) << /Hi 34.35 >>]").unwrap();
        assert!(matches!(res[0], Object::String(_)));
        assert!(matches!(res[1], Object::Dict(_)));
    }

    #[test]
    fn array_5() {
        let res = array_impl(b"[[32]  345.6]").unwrap();
        assert!(matches!(res[0], Object::Array(_)));
        assert!(matches!(res[1], Object::Number(_)));
    }

    #[test]
    fn array_with_single_number_is_valid() {
        assert!(array_ref_impl(b"[345345345]").is_some());
    }

    #[test]
    fn array_with_comment() {
        let res = array_impl(b"[true % A comment \n false]").unwrap();
        assert!(matches!(res[0], Object::Boolean(_)));
        assert!(matches!(res[1], Object::Boolean(_)));
    }

    #[test]
    fn array_with_trailing() {
        let res = array_impl(b"[(Hi) /Test]trialing data").unwrap();
        assert!(matches!(res[0], Object::String(_)));
        assert!(matches!(res[1], Object::Name(_)));
    }

    fn numbers(data: &[u8]) -> Array<'_> {
        Reader::new(data)
            .read_with_context::<Array<'_>>(&ReaderContext::new(XRef::dummy(), false))
            .unwrap()
    }

    #[test]
    fn iter_skips_an_entry_of_another_type_or_a_reference_to_no_object() {
        let array = numbers(b"[1 null 2 (x) 5 0 R 3 /N]");
        assert_eq!(array.iter::<f32>().collect::<Vec<_>>(), [1.0, 2.0, 3.0]);
    }

    #[test]
    fn items_keep_each_entry_in_its_place() {
        let array = numbers(b"[1 null 5 0 R [2] 2]");
        assert_eq!(
            array.items::<f32>().collect::<Vec<_>>(),
            [Some(1.0), None, None, None, Some(2.0)]
        );
    }

    #[test]
    fn a_vec_is_read_up_to_the_first_entry_of_another_type() {
        let array = numbers(b"[1 2 null 3]");
        assert_eq!(Vec::<f32>::try_from(array.clone()), Ok(vec![1.0, 2.0]));
        assert_eq!(<[f32; 3]>::try_from(array), Err(()));
    }

    /// Each array of `cases`, as read from its bytes and as parsed once.
    fn both_readings(f: impl Fn(&[u8], &Array<'_>)) {
        let cases: [&[u8]; 14] = [
            b"[]",
            b"[1]",
            b"[1 2]",
            b"[1 2 3]",
            b"[1 null 2]",
            b"[1 /N 2 3]",
            b"[1 2 (x) 3]",
            b"[1.5 2]",
            b"[-1 70000 2 3]",
            b"[1 5 0 R 2 3]",
            b"[[1] 2]",
            b"[1 2 3 4 5 6 7 8 9 10]",
            b"[0 1 % a comment\n 2]",
            b"[1 2 3 4 /N]",
        ];
        let ctx = ReaderContext::new(XRef::dummy(), false);
        for data in cases {
            f(data, &numbers(data));
            let parsed = crate::object::parsed::Top::read(&mut Reader::new(data), data, &ctx)
                .unwrap()
                .object(data, &ctx);
            let Object::Array(parsed) = parsed else {
                panic!("{data:?} is no array");
            };
            f(data, &parsed);
        }
    }

    #[test]
    fn leading_entries_read_as_the_whole_array_does() {
        use crate::work::Meter;
        use smallvec::SmallVec;
        both_readings(|data, array| {
            for n in 0..6 {
                let meter = Meter::default();
                let exactly = array.leading::<f32>(n, &meter).unwrap().exactly();
                let whole = SmallVec::<[f32; 4]>::try_from(array.clone())
                    .ok()
                    .filter(|v| v.len() == n);
                assert_eq!(exactly, whole, "{data:?} exactly {n}");
                let at_least = array.leading::<u16>(n, &meter).unwrap().at_least();
                let mut entries = array.flex_iter();
                let prefix: Option<SmallVec<[u16; 4]>> =
                    (0..n).map(|_| entries.next::<u16>()).collect();
                assert_eq!(at_least, prefix, "{data:?} at least {n}");
                let leading = array.leading::<f32>(n, &meter).unwrap().entries;
                let items: Vec<_> = array.items::<f32>().take(n).collect();
                assert_eq!(leading.as_slice(), items.as_slice(), "{data:?} entries {n}");
            }
        });
    }

    #[test]
    fn leading_entries_are_counted_and_stop() {
        use crate::work::Meter;
        let array = numbers(b"[1 2 3 4 5 6 7 8]");
        let polls = core::cell::Cell::new(0);
        let hook = || {
            polls.set(polls.get() + 1);
            true
        };
        let meter = Meter::new(Some(&hook));
        let mut read = 0;
        while array.leading::<f32>(3, &meter).is_ok() {
            read += 1;
        }
        assert!(
            read > 0 && polls.get() == 1,
            "{read} reads, {} polls",
            polls.get()
        );
    }

    #[test]
    fn array_repr() {
        let array = Array::from_bytes(b"[34]").unwrap();

        assert_eq!(format!("{array:?}"), "[Number(Number(Integer(34)))]");
    }

    #[test]
    fn display() {
        let array = Array::from_bytes(b"[  34 % comment\n /Test  [1   2] ]").unwrap();

        assert_eq!(format!("{array}"), "[34 /Test [1 2]]");
    }

    #[test]
    fn array_with_unreadable_item() {
        let _ = array_ref_impl(b"[true /#zz false]");
    }
}
