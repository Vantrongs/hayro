//! Copying a stream, such as an annotation's appearance, as a form `XObject`.

use hayro_write::hayro_syntax::Pdf;
use hayro_write::hayro_syntax::object::{Dict, Name, ObjRef, Rect, Stream};
use hayro_write::pdf_writer::{self, Content, Finish, Ref};
use hayro_write::{ChunkSettings, ExtractionError, ExtractionQuery, extract};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

const APPEARANCE: &[u8] = b"BT /Helv 9 Tf 2 3 Td (Approved) Tj ET";

/// A page with one stamp annotation (object 4) whose normal appearance (object 5)
/// is a stream with neither /Type nor /Subtype and a font in its resources (6).
fn annotated() -> Pdf {
    let file = fixtures::one_page(
        200,
        "/Annots [4 0 R]",
        vec![
            b"<< /Type /Annot /Subtype /Stamp /Rect [10 10 90 40] /AP << /N 5 0 R >> >>".to_vec(),
            fixtures::stream(
                "/BBox [0 0 80 30] /Matrix [1 0 0 1 5 5] /Resources << /Font << /Helv 6 0 R >> >>",
                APPEARANCE,
            ),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ],
    );
    Pdf::new(file).expect("valid pdf")
}

/// A file whose one page draws each extracted root as `/X0`, `/X1`, ….
fn draw_roots(
    pdf: &Pdf,
    queries: &[ExtractionQuery],
) -> (Vec<u8>, Vec<Result<Ref, ExtractionError>>) {
    let mut next = Ref::new(1);
    let (catalog, tree) = (next.bump(), next.bump());
    let extracted = extract(
        pdf,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        queries,
    )
    .expect("extraction");
    let (page, content) = (next.bump(), next.bump());
    let mut out = pdf_writer::Pdf::new();
    out.extend(&extracted.chunk);
    out.catalog(catalog).pages(tree);
    out.pages(tree).kids([page]).count(1);
    let names: Vec<String> = (0..queries.len()).map(|i| format!("X{i}")).collect();
    let mut c = Content::new();
    let mut p = out.page(page);
    p.media_box(pdf_writer::Rect::new(0.0, 0.0, 200.0, 200.0))
        .parent(tree)
        .contents(content);
    let mut resources = p.resources();
    let mut x_objects = resources.x_objects();
    for (name, root) in names.iter().zip(&extracted.root_refs) {
        if let Ok(root) = root {
            x_objects.pair(pdf_writer::Name(name.as_bytes()), *root);
            c.x_object(pdf_writer::Name(name.as_bytes()));
        }
    }
    x_objects.finish();
    resources.finish();
    p.finish();
    out.stream(content, &c.finish());
    (out.finish(), extracted.root_refs)
}

#[test]
fn form_copies_an_appearance_stream_as_a_typed_form() {
    let source = annotated();
    let (file, roots) = draw_roots(&source, &[ExtractionQuery::new_form(ObjRef::new(5, 0), 0)]);
    assert!(roots[0].is_ok());

    let copy = Pdf::new(file).expect("the copy parses");
    let page = &copy.pages()[0];
    let form = page
        .resources()
        .x_objects
        .get::<Stream<'_>>("X0")
        .expect("the form is in the page's resources");
    let dict = form.dict();
    assert_eq!(dict.get::<Name<'_>>(b"Type").unwrap().as_ref(), b"XObject");
    assert_eq!(dict.get::<Name<'_>>(b"Subtype").unwrap().as_ref(), b"Form");
    let bbox = dict.get::<Rect>(b"BBox").unwrap();
    assert_eq!([bbox.x0, bbox.y0, bbox.x1, bbox.y1], [0.0, 0.0, 80.0, 30.0]);
    assert_eq!(
        dict.get::<[f64; 6]>(b"Matrix"),
        Some([1.0, 0.0, 0.0, 1.0, 5.0, 5.0])
    );
    let font = dict
        .get::<Dict<'_>>(b"Resources")
        .and_then(|r| r.get::<Dict<'_>>(b"Font"))
        .and_then(|f| f.get::<Dict<'_>>(b"Helv"))
        .expect("the font the appearance uses is copied with it");
    assert_eq!(
        font.get::<Name<'_>>(b"BaseFont").unwrap().as_ref(),
        b"Helvetica"
    );
    assert_eq!(form.decoded().unwrap().as_ref(), APPEARANCE);
}

#[test]
fn form_keeps_a_type_the_stream_already_has() {
    let file = fixtures::one_page(
        200,
        "",
        vec![fixtures::stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 10 10]",
            b"0 0 10 10 re f",
        )],
    );
    let source = Pdf::new(file).unwrap();
    let (file, _) = draw_roots(&source, &[ExtractionQuery::new_form(ObjRef::new(4, 0), 0)]);
    // pdf-writer would have written the keys twice had the copy not left them out.
    let text = String::from_utf8_lossy(&file);
    assert_eq!(text.matches("/Subtype /Form").count(), 1);
    assert_eq!(text.matches("/Type /XObject").count(), 1);
}

#[test]
fn form_of_something_other_than_a_stream_is_an_error() {
    let source = annotated();
    let (_, roots) = draw_roots(
        &source,
        &[
            ExtractionQuery::new_form(ObjRef::new(4, 0), 0),
            ExtractionQuery::new_form(ObjRef::new(99, 0), 0),
            ExtractionQuery::new_form(ObjRef::new(5, 0), 0),
        ],
    );
    assert!(matches!(
        roots[0],
        Err(ExtractionError::NotAStream(ObjRef {
            obj_number: 4,
            gen_number: 0
        }))
    ));
    assert!(matches!(roots[1], Err(ExtractionError::NotAStream(_))));
    assert!(roots[2].is_ok(), "one bad query does not stop the others");
}
