use crate::object::ObjectIdentifier;
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
    T: Readable<'a>,
{
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let mut ctx = ctx.clone();
        let id = r.read_without_context::<ObjectIdentifier>()?;

        if ctx.parent_chain_contains(&id) {
            return None;
        }

        ctx.set_obj_number(id);
        ctx.parent_chain_push(id);
        r.skip_white_spaces_and_comments();
        let inner = r.read_indirect::<T>(&ctx)?;
        r.skip_white_spaces_and_comments();
        // We are lenient and don't require it.
        r.forward_tag(b"endobj");

        Some(Self { inner })
    }
}
