//! What a copy keeps of the source: every resource a form draws with, all of a
//! page's content or an error, one copy of repeated streams, and no encryption.

use hayro_write::hayro_syntax::Pdf;
use hayro_write::hayro_syntax::object::{Dict, Name, ObjRef, Rect, Stream};
use hayro_write::hayro_syntax::page::ContentError;
use hayro_write::pdf_writer::{self, Ref};
use hayro_write::{
    ChunkSettings, ExtractionError, ExtractionQuery, ExtractionResult, FormPlacement, extract,
};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

fn run(pdf: &Pdf, queries: &[ExtractionQuery]) -> Result<ExtractionResult, ExtractionError> {
    let mut next = Ref::new(1);
    extract(
        pdf,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        queries,
    )
}

/// The chunk in a file without pages, to read its objects back.
fn reread(result: &ExtractionResult) -> Pdf {
    let (catalog, tree) = (Ref::new(100_000), Ref::new(100_001));
    let mut out = pdf_writer::Pdf::new();
    out.extend(&result.chunk);
    out.catalog(catalog).pages(tree);
    out.pages(tree).count(0);
    Pdf::new(out.finish()).expect("the copy parses")
}

fn get<'a>(pdf: &'a Pdf, r: Ref) -> Stream<'a> {
    pdf.xref()
        .get::<Stream<'_>>(ObjRef::new(r.get(), 0).into())
        .expect("a stream")
}

/// The box and matrix of the appearance of [`shared_stamp`].
const AT: FormPlacement = FormPlacement {
    bbox: [0.0, 0.0, 50.0, 50.0],
    matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
};

/// Two pages that draw the same stamp, whose appearance (object 7) has no
/// resources and draws `/Logo`, which each page's resources give differently.
fn shared_stamp(appearance_entries: &str) -> Pdf {
    let page = |logo: u32| {
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [6 0 R] \
             /Resources << /XObject << /Logo {logo} 0 R >> >> >>"
        )
        .into_bytes()
    };
    let logo = |color: &str| {
        fixtures::stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 50 50]",
            format!("{color} rg 0 0 50 50 re f").as_bytes(),
        )
    };
    Pdf::new(fixtures::file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
            page(8),
            page(9),
            b"null".to_vec(),
            b"<< /Type /Annot /Subtype /Stamp /Rect [0 0 50 50] /AP << /N 7 0 R >> >>".to_vec(),
            fixtures::stream(
                &format!("/BBox [0 0 50 50] {appearance_entries}"),
                b"/Logo Do",
            ),
            logo("1 0 0"),
            logo("0 1 0"),
        ],
        "",
    ))
    .unwrap()
}

#[test]
fn a_form_without_resources_takes_its_page_s() {
    let source = shared_stamp("");
    let appearance = ObjRef::new(7, 0);
    let queries = [0, 1, 0].map(|page| ExtractionQuery::new_form(appearance, page, AT));
    let result = run(&source, &queries).unwrap();
    let roots: Vec<Ref> = result.root_refs.iter().map(|r| r.unwrap()).collect();
    assert_ne!(roots[0], roots[1], "one copy for each page's resources");
    assert_eq!(roots[0], roots[2], "one copy for each page");
    let copy = reread(&result);
    let colors: Vec<Vec<u8>> = roots[..2]
        .iter()
        .map(|&root| {
            let logo = get(&copy, root)
                .dict()
                .get::<Dict<'_>>(b"Resources")
                .and_then(|r| r.get::<Dict<'_>>(b"XObject"))
                .and_then(|x| x.get::<Stream<'_>>(b"Logo"))
                .expect("the page's /Logo comes with the appearance");
            logo.decoded().unwrap().to_vec()
        })
        .collect();
    assert_eq!(colors[0], b"1 0 0 rg 0 0 50 50 re f");
    assert_eq!(colors[1], b"0 1 0 rg 0 0 50 50 re f");
}

#[test]
fn a_form_with_its_own_resources_is_copied_once() {
    let source = shared_stamp("/Resources << /XObject << /Logo 8 0 R >> >>");
    let appearance = ObjRef::new(7, 0);
    let queries = [0, 1].map(|page| ExtractionQuery::new_form(appearance, page, AT));
    let result = run(&source, &queries).unwrap();
    let roots: Vec<Ref> = result.root_refs.iter().map(|r| r.unwrap()).collect();
    assert_eq!(roots[0], roots[1]);
}

/// One page drawing two images whose dictionaries refer to nothing, and a third
/// that refers to a color space.
fn repeated_streams(second: &[u8]) -> Pdf {
    let image = |data: &[u8], color_space: &str| {
        fixtures::stream(
            &format!(
                "/Type /XObject /Subtype /Image /Width 2 /Height 1 /BitsPerComponent 8 /ColorSpace {color_space}"
            ),
            data,
        )
    };
    Pdf::new(fixtures::one_page(
        200,
        "/Resources << /XObject << /A 4 0 R /B 5 0 R /C 6 0 R /D 7 0 R >> >> /Contents 9 0 R",
        vec![
            image(b"\x00\xff", "/DeviceGray"),
            image(second, "/DeviceGray"),
            image(b"\x00\xff", "8 0 R"),
            image(b"\x00\xff", "8 0 R"),
            b"/DeviceGray".to_vec(),
            fixtures::stream("", b"/A Do /B Do /C Do /D Do"),
        ],
    ))
    .unwrap()
}

/// The copies the entries of the page's `/XObject` dictionary refer to.
fn image_refs(source: &Pdf) -> Vec<Ref> {
    let result = run(source, &[ExtractionQuery::new_page(0)]).unwrap();
    let text = String::from_utf8_lossy(result.chunk.as_bytes()).into_owned();
    ["A", "B", "C", "D"]
        .iter()
        .map(|name| {
            let at = text.find(&format!("/{name} ")).unwrap() + name.len() + 2;
            let number = text[at..].split(' ').next().unwrap();
            Ref::new(number.parse().unwrap())
        })
        .collect()
}

#[test]
fn equal_streams_that_refer_to_nothing_are_copied_once() {
    let refs = image_refs(&repeated_streams(b"\x00\xff"));
    assert_eq!(refs[0], refs[1]);
    // Streams that refer to other objects are copied each.
    assert_ne!(refs[2], refs[3]);
    assert_ne!(refs[0], refs[2]);
    let refs = image_refs(&repeated_streams(b"\xff\x00"));
    assert_ne!(refs[0], refs[1]);
}

#[test]
fn a_user_space_form_has_the_callers_box_and_no_matrix() {
    let source = Pdf::new(fixtures::one_page(
        200,
        "/CropBox [100 100 100 100] /Rotate 90 /Contents 4 0 R",
        vec![fixtures::stream("", b"0 0 200 200 re f")],
    ))
    .unwrap();
    let query = ExtractionQuery::new_user_space_xobject(0, [-10.0, 0.0, 200.0, 210.0]);
    let result = run(&source, &[query]).unwrap();
    let copy = reread(&result);
    let form = get(&copy, result.root_refs[0].unwrap());
    let bbox = form.dict().get::<Rect>(b"BBox").unwrap();
    assert_eq!(
        [bbox.x0, bbox.y0, bbox.x1, bbox.y1],
        [-10.0, 0.0, 200.0, 210.0]
    );
    assert!(form.dict().get::<[f32; 6]>(b"Matrix").is_none());
    assert_eq!(form.decoded().unwrap().as_ref(), b"0 0 200 200 re f");
}

#[test]
fn crypt_filters_are_left_out_of_the_copy() {
    // An unencrypted file, where a reader passes /Crypt data through.
    let data = fixtures::stored_zlib(b"0 0 10 10 re f", 64);
    let source = Pdf::new(fixtures::one_page(
        200,
        "/Resources << /XObject << /A 4 0 R /B 5 0 R >> >>",
        vec![
            fixtures::stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 9 9] /Filter /Crypt \
                 /DecodeParms << /Name /Identity >>",
                b"0 0 10 10 re f",
            ),
            fixtures::stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 9 9] /Filter [/Crypt /FlateDecode] \
                 /DecodeParms [<< /Name /Identity >> null]",
                &data,
            ),
        ],
    ))
    .unwrap();
    let result = run(&source, &[ExtractionQuery::new_page(0)]).unwrap();
    let copy = reread(&result);
    let text = String::from_utf8_lossy(result.chunk.as_bytes()).into_owned();
    assert!(!text.contains("Crypt"), "{text}");
    assert!(!text.contains("Identity"), "{text}");
    let mut decoded = Vec::new();
    for number in 1..20 {
        if let Some(stream) = copy.xref().get::<Stream<'_>>(ObjRef::new(number, 0).into())
            && stream.dict().get::<Name<'_>>(b"Subtype").as_deref() == Some(b"Form".as_slice())
        {
            decoded.push(stream.decoded().unwrap().to_vec());
        }
    }
    assert_eq!(
        decoded,
        [b"0 0 10 10 re f".to_vec(), b"0 0 10 10 re f".to_vec()]
    );
}

#[test]
fn a_page_whose_content_cannot_be_read_is_an_error() {
    let source = Pdf::new(fixtures::one_page(
        200,
        "/Contents [4 0 R 5 0 R]",
        vec![
            fixtures::stream("", b"0 0 10 10 re f"),
            fixtures::stream(
                "/Filter /FlateDecode",
                b"not flate at all, not flate at all",
            ),
        ],
    ))
    .unwrap();
    for query in [
        ExtractionQuery::new_xobject(0),
        ExtractionQuery::new_user_space_xobject(0, [0.0, 0.0, 200.0, 200.0]),
        ExtractionQuery::new_page(0),
    ] {
        let result = run(&source, &[query]).unwrap();
        assert!(
            matches!(
                result.root_refs[0],
                Err(ExtractionError::Content(0, ContentError::Decode(Some(id), _)))
                    if id.obj_number == 5
            ),
            "{:?}",
            result.root_refs[0]
        );
    }
}
