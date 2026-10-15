use crate::{ExtractionContext, ExtractionError};
use hayro_syntax::hash;
use hayro_syntax::object;
use hayro_syntax::object::dict::keys::{
    AF, CRYPT, DECODE_PARMS, FILTER, LAST_MODIFIED, LENGTH, METADATA, OC, OPI, PIECE_INFO, PT_DATA,
    REF, STRUCT_PARENT, STRUCT_PARENTS,
};
use hayro_syntax::object::{MaybeRef, Null, Number, ObjectIdentifier, Stream};
use hayro_syntax::object::{Object, array, dict};
use pdf_writer::{Chunk, Dict, Finish, Obj, Ref};
use std::hash::BuildHasher;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::LazyLock;

static IGNORE_KEYS: LazyLock<hash::Set<&'static [u8]>> = LazyLock::new(|| {
    let mut m = hash::Set::default();

    m.insert(METADATA);
    m.insert(STRUCT_PARENT);
    m.insert(OC);
    m.insert(AF);
    m.insert(PT_DATA);
    m.insert(REF);
    m.insert(LAST_MODIFIED);
    m.insert(PIECE_INFO);
    m.insert(STRUCT_PARENTS);
    m.insert(OPI);

    m
});

pub(crate) trait WriteDirect {
    fn write_direct(&self, obj: Obj<'_>, _: &mut ExtractionContext<'_>);
}

impl WriteDirect for object::ObjRef {
    fn write_direct(&self, obj: Obj<'_>, ctx: &mut ExtractionContext<'_>) {
        match target(*self, ctx) {
            Some(mapped_ref) => obj.primitive(mapped_ref),
            None => obj.primitive(pdf_writer::Null),
        }
    }
}

/// What a reference to `source` is written as, queuing the body it leads to for
/// copying the first time an entry leading there is referred to: one copy for every
/// entry leading to the body ([`ExtractionContext::copies`]).
///
/// A reference to an object that does not exist is `null`, as it reads: writing it
/// would leave a reference to nothing. One the file lists but that cannot be read
/// is lost, which fails the extraction ([`crate::extract`]). A stream whose
/// dictionary refers to no other object is copied once for all streams with the same
/// copied dictionary and data, its data hashed once, with its body.
fn target<'a>(source: object::ObjRef, ctx: &mut ExtractionContext<'a>) -> Option<Ref> {
    let pdf = ctx.pdf;
    let id = ObjectIdentifier::new(source.obj_number, source.gen_number);
    let body = pdf.xref().content_key(id)?;
    if let Some(copy) = ctx.copies.get(&body) {
        return *copy;
    }
    let copy = first_target(source, ctx);
    ctx.copies.insert(body, copy);
    copy
}

/// [`target`] for the first reference to an entry leading to its body.
fn first_target(source: object::ObjRef, ctx: &mut ExtractionContext<'_>) -> Option<Ref> {
    let pdf = ctx.pdf;
    let id = ObjectIdentifier::new(source.obj_number, source.gen_number);
    let object = pdf.xref().get::<Object<'_>>(id)?;
    if let Object::Stream(stream) = &object
        && !refers(stream.dict())
    {
        let data = match stream.try_raw_data() {
            Ok(data) => data,
            Err(failure) => {
                ctx.fail(ExtractionError::StreamData(source, failure));
                return None;
            }
        };
        let mut scratch = Chunk::new();
        let mut dict = scratch.indirect(Ref::new(1)).dict();
        write_stream_dict(stream.dict(), &mut dict, ctx, &[]);
        dict.finish();
        let _ = ctx.meter.work(scratch.len() + data.len());
        // Hashed with the map's random keys, as the key it is part of is.
        let hashed = ctx.shared_streams.hasher().hash_one(&*data);
        let key = (scratch.as_bytes().to_vec(), data.len(), hashed);
        let copy = match ctx.shared_streams.get(&key) {
            Some((other, copy)) => {
                // One comparison for each stream, however many a file makes hash
                // alike: one whose data differ is copied on its own.
                let _ = ctx.meter.work(data.len());
                let other = pdf.xref().get::<Stream<'_>>((*other).into());
                if other.and_then(|o| o.try_raw_data().ok()).as_deref() == Some(&*data) {
                    return Some(*copy);
                }
                ctx.new_ref()
            }
            None => {
                let copy = ctx.new_ref();
                ctx.shared_streams.insert(key, (source, copy));
                copy
            }
        };
        ctx.to_copy.push((source, copy, ctx.page));
        return Some(copy);
    }
    let copy = ctx.new_ref();
    ctx.to_copy.push((source, copy, ctx.page));
    Some(copy)
}

/// Whether the copy of `dict` would refer to another object.
fn refers(dict: &dict::Dict<'_>) -> bool {
    fn object_refers(object: &MaybeRef<Object<'_>>) -> bool {
        match object {
            MaybeRef::Ref(_) => true,
            MaybeRef::NotRef(Object::Dict(dict)) => refers(dict),
            MaybeRef::NotRef(Object::Array(array)) => array.raw_iter().any(|o| object_refers(&o)),
            MaybeRef::NotRef(_) => false,
        }
    }

    dict.entries().any(|(name, value)| {
        !IGNORE_KEYS.contains(name.deref())
            && !STREAM_KEYS.contains(&name.deref())
            && object_refers(&value)
    })
}

impl WriteDirect for Number {
    fn write_direct(&self, obj: Obj<'_>, _: &mut ExtractionContext<'_>) {
        let float_num = self.as_f64();

        if float_num.fract() == 0.0 {
            obj.primitive(float_num as i32);
        } else {
            obj.primitive(float_num as f32);
        }
    }
}

impl WriteDirect for bool {
    fn write_direct(&self, obj: Obj<'_>, _: &mut ExtractionContext<'_>) {
        obj.primitive(self);
    }
}

impl WriteDirect for Null {
    fn write_direct(&self, obj: Obj<'_>, _: &mut ExtractionContext<'_>) {
        obj.primitive(pdf_writer::Null);
    }
}

impl WriteDirect for object::String<'_> {
    fn write_direct(&self, obj: Obj<'_>, _: &mut ExtractionContext<'_>) {
        obj.primitive(pdf_writer::Str(self.as_ref()));
    }
}

impl WriteDirect for object::Name<'_> {
    fn write_direct(&self, obj: Obj<'_>, _: &mut ExtractionContext<'_>) {
        obj.primitive(pdf_writer::Name(self.deref()));
    }
}

impl WriteDirect for object::Array<'_> {
    fn write_direct(&self, obj: Obj<'_>, ctx: &mut ExtractionContext<'_>) {
        let mut arr = obj.array();
        for item in self.raw_iter() {
            let obj = arr.push();
            item.write_direct(obj, ctx);
        }
    }
}

impl<T: WriteDirect> WriteDirect for MaybeRef<T> {
    fn write_direct(&self, obj: Obj<'_>, ctx: &mut ExtractionContext<'_>) {
        match self {
            Self::Ref(r) => r.write_direct(obj, ctx),
            Self::NotRef(o) => o.write_direct(obj, ctx),
        }
    }
}

/// The entries of a stream dictionary that its writer sets itself.
pub(crate) const STREAM_KEYS: &[&[u8]] = &[LENGTH];

/// Copies `hayro_dict` into `pdf_dict`, leaving out the ignored keys and `skip`.
pub(crate) fn write_dict(
    hayro_dict: &dict::Dict<'_>,
    pdf_dict: &mut Dict<'_>,
    ctx: &mut ExtractionContext<'_>,
    skip: &[&[u8]],
) {
    for (name, val) in hayro_dict.entries() {
        if !IGNORE_KEYS.contains(name.deref()) && !skip.contains(&name.deref()) {
            val.write_direct(pdf_dict.insert(pdf_writer::Name(name.deref())), ctx);
        }
    }
}

impl WriteDirect for object::Dict<'_> {
    fn write_direct(&self, obj: Obj<'_>, ctx: &mut ExtractionContext<'_>) {
        let mut dict = obj.dict();

        write_dict(self, &mut dict, ctx, &[]);
    }
}

impl WriteDirect for Object<'_> {
    fn write_direct(&self, obj: Obj<'_>, ctx: &mut ExtractionContext<'_>) {
        match self {
            Object::Null(n) => n.write_direct(obj, ctx),
            Object::Boolean(b) => b.write_direct(obj, ctx),
            Object::Number(n) => n.write_direct(obj, ctx),
            Object::String(s) => s.write_direct(obj, ctx),
            Object::Name(n) => n.write_direct(obj, ctx),
            Object::Dict(d) => d.write_direct(obj, ctx),
            Object::Array(a) => a.write_direct(obj, ctx),
            Object::Stream(_) => unreachable!(),
        }
    }
}

pub(crate) trait WriteIndirect {
    fn write_indirect(&self, chunk: &mut Chunk, id: Ref, ctx: &mut ExtractionContext<'_>);
}

macro_rules! write_indirect {
    ($name:ty) => {
        impl WriteIndirect for $name {
            fn write_indirect(&self, chunk: &mut Chunk, id: Ref, ctx: &mut ExtractionContext<'_>) {
                self.write_direct(chunk.indirect(id), ctx);
            }
        }
    };
}

write_indirect!(Null);
write_indirect!(bool);
write_indirect!(Number);
write_indirect!(object::String<'_>);
write_indirect!(object::Name<'_>);
write_indirect!(dict::Dict<'_>);
write_indirect!(array::Array<'_>);

impl WriteIndirect for Stream<'_> {
    fn write_indirect(&self, chunk: &mut Chunk, id: Ref, ctx: &mut ExtractionContext<'_>) {
        let data = match self.try_raw_data() {
            Ok(data) => data,
            Err(failure) => {
                let id = self.obj_id();
                ctx.fail(ExtractionError::StreamData(id.into(), failure));
                return;
            }
        };
        let mut obj = chunk.stream(id, &data);
        write_stream_dict(self.dict(), obj.deref_mut(), ctx, &[]);
    }
}

/// Copies the dictionary of a stream whose data is copied decrypted, leaving out
/// `skip`, the length and any /Crypt filter with its parameters: the copy is not
/// encrypted.
pub(crate) fn write_stream_dict(
    hayro_dict: &dict::Dict<'_>,
    pdf_dict: &mut Dict<'_>,
    ctx: &mut ExtractionContext<'_>,
    skip: &[&[u8]],
) {
    let is_crypt = |name: &object::Name<'_>| name.deref() == CRYPT;
    let filters = hayro_dict.get::<Object<'_>>(FILTER);
    let crypt = match &filters {
        Some(Object::Name(name)) => is_crypt(name),
        Some(Object::Array(filters)) => filters.iter::<object::Name<'_>>().any(|n| is_crypt(&n)),
        _ => false,
    };
    let mut skipped = STREAM_KEYS.to_vec();
    skipped.extend_from_slice(skip);
    if !crypt {
        write_dict(hayro_dict, pdf_dict, ctx, &skipped);
        return;
    }
    skipped.extend_from_slice(&[FILTER, DECODE_PARMS]);
    write_dict(hayro_dict, pdf_dict, ctx, &skipped);
    // Only an array of filters keeps some; the parameters keep their places.
    let Some(Object::Array(filters)) = filters else {
        return;
    };
    let params = hayro_dict.get::<array::Array<'_>>(DECODE_PARMS);
    let kept: Vec<usize> = filters
        .iter::<Object<'_>>()
        .enumerate()
        .filter(|(_, f)| !matches!(f, Object::Name(n) if is_crypt(n)))
        .map(|(i, _)| i)
        .collect();
    if kept.is_empty() {
        return;
    }
    let mut out = pdf_dict.insert(pdf_writer::Name(FILTER)).array();
    for (i, filter) in filters.raw_iter().enumerate() {
        if kept.contains(&i) {
            filter.write_direct(out.push(), ctx);
        }
    }
    out.finish();
    if let Some(params) = params {
        let mut out = pdf_dict.insert(pdf_writer::Name(DECODE_PARMS)).array();
        for (i, param) in params.raw_iter().enumerate() {
            if kept.contains(&i) {
                param.write_direct(out.push(), ctx);
            }
        }
    }
}

impl WriteIndirect for Object<'_> {
    fn write_indirect(&self, chunk: &mut Chunk, id: Ref, ctx: &mut ExtractionContext<'_>) {
        match self {
            Object::Null(n) => n.write_indirect(chunk, id, ctx),
            Object::Boolean(b) => b.write_indirect(chunk, id, ctx),
            Object::Number(n) => n.write_indirect(chunk, id, ctx),
            Object::String(s) => s.write_indirect(chunk, id, ctx),
            Object::Name(n) => n.write_indirect(chunk, id, ctx),
            Object::Dict(d) => d.write_indirect(chunk, id, ctx),
            Object::Array(a) => a.write_indirect(chunk, id, ctx),
            Object::Stream(s) => s.write_indirect(chunk, id, ctx),
        }
    }
}
