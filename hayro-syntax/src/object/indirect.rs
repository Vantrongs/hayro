use crate::object::{ObjectIdentifier, ObjectLike};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt};

#[derive(Debug, Clone)]
pub(crate) struct IndirectObject<T> {
    inner: T,
}

impl<T> IndirectObject<T> {
    pub(crate) fn get(self) -> T {
        self.inner
    }
}

impl<'a, T> Readable<'a> for IndirectObject<T>
where
    T: ObjectLike<'a>,
{
    // The object is read in `ctx`, which names the object the table lists here
    // (`XRef::get_with`): the header may name another, and is no identity, though it
    // names the key the object decrypts with, which `ctx` holds.
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        r.skip::<ObjectIdentifier>(false)?;
        r.skip_white_spaces_and_comments();
        let inner = T::read_indirect(r, ctx)?;
        r.skip_white_spaces_and_comments();
        // We are lenient and don't require it.
        r.forward_tag(b"endobj");

        Some(Self { inner })
    }
}
