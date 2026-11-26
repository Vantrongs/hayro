use crate::{ExtractionContext, ExtractionError};
use hayro_syntax::Filter;
use hayro_syntax::hash;
use hayro_syntax::object;
use hayro_syntax::object::dict::keys::{
    AF, DECODE_PARMS, DP, FILTER, LAST_MODIFIED, LENGTH, METADATA, OPI, PIECE_INFO, PT_DATA, REF,
    STRUCT_PARENT, STRUCT_PARENTS,
};
use hayro_syntax::object::{MaybeRef, ObjectIdentifier, Stream, Written};
use hayro_syntax::object::{Object, dict};
use pdf_writer::{Chunk, Dict, Finish, NumberToken, Obj, Ref};
use std::hash::BuildHasher;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::LazyLock;

static IGNORE_KEYS: LazyLock<hash::Set<&'static [u8]>> = LazyLock::new(|| {
    let mut m = hash::Set::default();

    m.insert(METADATA);
    m.insert(STRUCT_PARENT);
    m.insert(AF);
    m.insert(PT_DATA);
    m.insert(REF);
    m.insert(LAST_MODIFIED);
    m.insert(PIECE_INFO);
    m.insert(STRUCT_PARENTS);
    m.insert(OPI);

    m
});

pub(crate) fn target<'a>(source: object::ObjRef, ctx: &mut ExtractionContext<'a>) -> Ref {
    let pdf = ctx.pdf;
    let id = ObjectIdentifier::new(source.obj_number, source.gen_number);
    let Some(body) = pdf.xref().content_key(id) else {
        return ctx.nothing();
    };
    if let Some(copy) = ctx.copies.get(&body) {
        return *copy;
    }
    let copy = first_target(source, ctx).unwrap_or_else(|| ctx.null());
    ctx.copies.insert(body, copy);
    copy
}

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
        write_stream_dict(stream, &mut dict, ctx, &[]);
        dict.finish();
        // Hashed with the map's random keys, as the key it is part of is.
        let hashed = ctx.shared_streams.hasher().hash_one(&*data);
        let key = (scratch.as_bytes().to_vec(), data.len(), hashed);
        let copy = match ctx.shared_streams.get(&key) {
            Some((other, copy)) => {
                // One comparison for each stream, however many a file makes hash
                // alike: one whose data differ is copied on its own.
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

pub(crate) fn write(value: &Written<'_>, obj: Obj<'_>, ctx: &mut ExtractionContext<'_>) {
    match value {
        Written::Ref(r) => obj.primitive(target(*r, ctx)),
        Written::Null => obj.primitive(pdf_writer::Null),
        Written::Boolean(b) => obj.primitive(*b),
        Written::Number(token) => obj.primitive(
            NumberToken::new(token).expect("a number is read from digits, signs and points"),
        ),
        Written::String(s) => obj.primitive(pdf_writer::Str(s.as_ref())),
        Written::Name(n) => obj.primitive(pdf_writer::Name(n.deref())),
        Written::Dict(d) => write_dict(d, &mut obj.dict(), ctx, &[]),
        Written::Array(a) => {
            let mut out = obj.array();
            for item in a.written() {
                write(&item, out.push(), ctx);
            }
        }
        // A stream is an object of its own (`write_indirect`): a dictionary in
        // another object followed by `stream` is read as a dictionary.
        Written::Stream(_) => unreachable!(),
    }
}

pub(crate) const STREAM_KEYS: &[&[u8]] = &[LENGTH];

pub(crate) fn write_dict(
    hayro_dict: &dict::Dict<'_>,
    pdf_dict: &mut Dict<'_>,
    ctx: &mut ExtractionContext<'_>,
    skip: &[&[u8]],
) {
    for (name, val) in hayro_dict.written() {
        if !IGNORE_KEYS.contains(name.deref()) && !skip.contains(&name.deref()) {
            write(&val, pdf_dict.insert(pdf_writer::Name(name.deref())), ctx);
        }
    }
}

pub(crate) fn write_indirect(
    value: &Written<'_>,
    chunk: &mut Chunk,
    id: Ref,
    ctx: &mut ExtractionContext<'_>,
) {
    let Written::Stream(stream) = value else {
        write(value, chunk.indirect(id), ctx);
        return;
    };
    let data = match stream.try_raw_data() {
        Ok(data) => data,
        Err(failure) => {
            let id = stream.obj_id();
            ctx.fail(ExtractionError::StreamData(id.into(), failure));
            return;
        }
    };
    let mut obj = chunk.stream(id, &data);
    write_stream_dict(stream, obj.deref_mut(), ctx, &[]);
}

pub(crate) fn write_stream_dict(
    stream: &Stream<'_>,
    pdf_dict: &mut Dict<'_>,
    ctx: &mut ExtractionContext<'_>,
    skip: &[&[u8]],
) {
    let hayro_dict = stream.dict();
    let filters = stream.filter_entries();
    let mut skipped = STREAM_KEYS.to_vec();
    skipped.extend_from_slice(skip);
    if !filters.applied().any(|(f, _)| f == Filter::Crypt) {
        write_dict(hayro_dict, pdf_dict, ctx, &skipped);
        return;
    }
    skipped.extend_from_slice(&[filters.key, FILTER, DECODE_PARMS, DP]);
    write_dict(hayro_dict, pdf_dict, ctx, &skipped);
    let kept = || {
        filters
            .entries()
            .filter(|e| e.filter != Some(Filter::Crypt))
    };
    if kept().next().is_none() {
        return;
    }
    let mut out = pdf_dict.insert(pdf_writer::Name(FILTER)).array();
    for entry in kept() {
        write(&entry.written, out.push(), ctx);
    }
    out.finish();
    if kept().any(|e| e.params_written.is_some()) {
        let mut out = pdf_dict.insert(pdf_writer::Name(DECODE_PARMS)).array();
        for entry in kept() {
            match &entry.params_written {
                Some(params) => write(params, out.push(), ctx),
                None => {
                    out.push().primitive(pdf_writer::Null);
                }
            }
        }
    }
}
