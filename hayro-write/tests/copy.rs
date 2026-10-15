//! What a copy keeps of the source: every resource a form draws with, all of a
//! page's content or an error, one copy of repeated streams, and no encryption.

use hayro_write::hayro_syntax::Pdf;
use hayro_write::hayro_syntax::object::{
    Array, Dict, Name, ObjRef, Object, Rect, Stream, Written, dict_or_stream,
};
use hayro_write::hayro_syntax::page::ContentError;
use hayro_write::pdf_writer::{self, Ref};
use hayro_write::{
    ChunkSettings, ExtractionError, ExtractionQuery, ExtractionResult, FormPlacement, extract,
};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;
#[path = "../../hayro-syntax/tests/common/rc4.rs"]
mod rc4;

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

/// A page content under a predictor whose last row is cut short is copied with that
/// row, as a page and as forms, as `MuPDF` draws it.
#[test]
fn a_content_row_cut_short_is_copied() {
    let source = Pdf::new(fixtures::one_page(
        100,
        "/Contents 4 0 R",
        vec![fixtures::cut_row_content_stream()],
    ))
    .unwrap();
    let result = run(&source, &page_queries(0, [100.0, 100.0])).unwrap();
    let copy = reread(&result);
    for root in &result.root_refs {
        let root = root.unwrap();
        let content = match copy
            .xref()
            .get::<Stream<'_>>(ObjRef::new(root.get(), 0).into())
        {
            Some(form) => form,
            None => copied_dict(&copy, root)
                .get::<Stream<'_>>(b"Contents")
                .unwrap(),
        };
        assert_eq!(
            content.decoded().unwrap().as_ref(),
            fixtures::CUT_ROW_CONTENT
        );
    }
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

/// A /Crypt filter left out of a copy takes its parameters with it, however the
/// filters and their parameters are written: each filter kept is read with the
/// parameters it was read with in the source, as `PyMuPDF` 1.28.2 reads them (an
/// entry of the filters that names none keeps its place; `/DP` is read before
/// `/DecodeParms`).
#[test]
fn crypt_filters_are_left_out_with_the_parameters_in_their_places() {
    const CONTENT: &[u8] = b"0 0 10 10 re f";
    // Two rows of 7 bytes under PNG filter 0.
    let rows = [b"\0".as_slice(), &CONTENT[..7], b"\0", &CONTENT[7..]].concat();
    let data = fixtures::stored_zlib(&rows, 64);
    let predictor = "<< /Predictor 12 /Columns 7 >>";
    let forms = [
        format!("/Filter [null /Crypt /FlateDecode] /DP [null null {predictor}]"),
        format!("/Filter [null /Crypt /FlateDecode] /DecodeParms [null null {predictor}]"),
        format!(
            "/Filter [/Crypt null /FlateDecode] /DP [<< /Name /Identity >> null {predictor}] \
             /DecodeParms [<< /Predictor 1 >>]"
        ),
        "/Filter [/Crypt /FlateDecode] /DecodeParms 9 0 R".to_string(),
        "/Filter [/Crypt 1 /FlateDecode] /DP [null 10 0 R 10 0 R]".to_string(),
    ];
    let mut objects: Vec<Vec<u8>> = forms
        .iter()
        .map(|entries| {
            fixtures::stream(
                &format!("/Type /XObject /Subtype /Form /BBox [0 0 9 9] {entries}"),
                &data,
            )
        })
        .collect();
    objects.push(format!("[null {predictor}]").into_bytes());
    objects.push(predictor.as_bytes().to_vec());
    let names: String = (0..forms.len())
        .map(|i| format!("/X{i} {} 0 R ", i + 4))
        .collect();
    let source = Pdf::new(fixtures::one_page(
        200,
        &format!("/Resources << /XObject << {names}>> >>"),
        objects,
    ))
    .unwrap();
    for number in 4..4 + forms.len() as i32 {
        let stream: Stream<'_> = source.xref().get(ObjRef::new(number, 0).into()).unwrap();
        assert_eq!(
            stream.decoded().unwrap().as_ref(),
            CONTENT,
            "source {number}"
        );
    }
    let result = run(&source, &[ExtractionQuery::new_page(0)]).unwrap();
    let copy = reread(&result);
    let text = String::from_utf8_lossy(result.chunk.as_bytes()).into_owned();
    assert!(!text.contains("Crypt"), "{text}");
    assert!(!text.contains("/DP"), "{text}");
    let x_objects = copied_dict(&copy, result.root_refs[0].unwrap())
        .get::<Dict<'_>>(b"Resources")
        .and_then(|r| r.get::<Dict<'_>>(b"XObject"))
        .expect("the copy draws the forms");
    for (i, entries) in forms.iter().enumerate() {
        let form = x_objects
            .get::<Stream<'_>>(format!("X{i}").as_bytes())
            .expect("a copied form");
        assert_eq!(form.decoded().unwrap().as_ref(), CONTENT, "{entries}");
    }
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

/// A file of `n` pages that inherit resources with `n` fonts, each in its own form.
fn copied_with_shared_fonts(n: usize) -> (Pdf, ExtractionResult) {
    let names: Vec<String> = (0..n).map(|i| format!("/F{i} {i}")).collect();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 3)).collect();
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!(
            "<< /Type /Pages /Count {n} /Kids [{}] /Resources << /Font << {} >> >> >>",
            kids.join(" "),
            names.join(" ")
        )
        .into_bytes(),
    ];
    let page = b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>".to_vec();
    objects.extend(std::iter::repeat_n(page, n));
    let source = Pdf::new(fixtures::file(&objects, "")).unwrap();
    let queries: Vec<_> = (0..n)
        .map(|page| ExtractionQuery::new_user_space_xobject(page, [0.0, 0.0, 200.0, 200.0]))
        .collect();
    let result = run(&source, &queries).unwrap();
    (reread(&result), result)
}

// Forms of pages that share their resources share one copy of them: copying `n`
// pages that share `n` fonts writes a file linear in `n`, and each form has them all.
#[test]
fn resources_pages_share_are_copied_once() {
    let [small, large] = [256, 512].map(|n| {
        let (copy, result) = copied_with_shared_fonts(n);
        for root in &result.root_refs {
            let fonts = get(&copy, root.unwrap())
                .dict()
                .get::<Dict<'_>>(b"Resources")
                .and_then(|resources| resources.get::<Dict<'_>>(b"Font"))
                .unwrap();
            assert_eq!(fonts.len(), n);
        }
        result.chunk.len()
    });
    assert!(
        large < small * 21 / 10,
        "{small} bytes for 256 pages, {large} for 512"
    );
}

/// A file of `n` pages that share one `/Font` category of `n` fonts, each page with
/// its own `/XObject` category, and the copy of each page as a form.
fn copied_with_own_xobjects(n: usize) -> (Pdf, ExtractionResult) {
    let fonts: Vec<String> = (0..n).map(|i| format!("/F{i} 4 0 R")).collect();
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 5 + 2 * i)).collect();
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" ")).into_bytes(),
        format!("<< {} >>", fonts.join(" ")).into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    for i in 0..n {
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font 3 0 R /XObject << /X{i} {} 0 R >> >> >>",
                6 + 2 * i
            )
            .into_bytes(),
        );
        objects.push(fixtures::stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 50 50]",
            b"0 0 50 50 re f",
        ));
    }
    let source = Pdf::new(fixtures::file(&objects, "")).unwrap();
    let queries: Vec<_> = (0..n)
        .map(|page| ExtractionQuery::new_user_space_xobject(page, [0.0, 0.0, 200.0, 200.0]))
        .collect();
    let result = run(&source, &queries).unwrap();
    (reread(&result), result)
}

// A resource category pages share is copied once, whatever the pages' other
// categories: copying `n` pages that share `n` fonts, each with its own form, writes a
// file linear in `n`, and each page has all the fonts and its own form.
#[test]
fn a_category_pages_share_is_copied_once() {
    let [small, large] = [256, 512].map(|n| {
        let (copy, result) = copied_with_own_xobjects(n);
        for (i, root) in result.root_refs.iter().enumerate() {
            let form = get(&copy, root.unwrap());
            let resources = form.dict().get::<Dict<'_>>(b"Resources").unwrap();
            assert_eq!(resources.get::<Dict<'_>>(b"Font").unwrap().len(), n);
            let x_objects = resources.get::<Dict<'_>>(b"XObject").unwrap();
            assert_eq!(x_objects.len(), 1);
            let own = x_objects.get::<Stream<'_>>(format!("X{i}").as_bytes());
            assert_eq!(own.unwrap().decoded().unwrap().as_ref(), b"0 0 50 50 re f");
        }
        result.chunk.len()
    });
    assert!(
        large < small * 21 / 10,
        "{small} bytes for 256 pages, {large} for 512"
    );
}

/// The dictionary of the copy's object `root`, a page's or a form's.
fn copied_dict(copy: &Pdf, root: Ref) -> Dict<'_> {
    let object = copy
        .xref()
        .get::<Object<'_>>(ObjRef::new(root.get(), 0).into())
        .expect("a copy");
    dict_or_stream(&object).expect("a page or a form").0.clone()
}

/// The queries copying page `page` whole, as a page and as forms.
fn page_queries(page: usize, size: [f32; 2]) -> [ExtractionQuery; 3] {
    [
        ExtractionQuery::new_page(page),
        ExtractionQuery::new_xobject(page),
        ExtractionQuery::new_user_space_xobject(page, [0.0, 0.0, size[0], size[1]]),
    ]
}

// Pages drawn with entries of an encrypted file's table that lead to one object are
// copied with that object's colors, as it was encrypted, copied alone or together.
#[test]
fn pages_drawn_with_entries_leading_to_one_object_are_copied_with_its_colors() {
    for whole in [true, false] {
        let source = Pdf::new(rc4::aliased(whole)).unwrap();
        for pages in [&[0][..], &[1], &[0, 1], &[1, 0]] {
            for kind in 0..3 {
                let queries: Vec<_> = pages
                    .iter()
                    .map(|&page| page_queries(page, [100.0, 100.0])[kind])
                    .collect();
                let result = run(&source, &queries).unwrap();
                let copy = reread(&result);
                for (&page, root) in pages.iter().zip(&result.root_refs) {
                    let space = copied_dict(&copy, root.unwrap())
                        .get::<Dict<'_>>(b"Resources")
                        .and_then(|r| r.get::<Dict<'_>>(b"ColorSpace"))
                        .and_then(|c| c.get::<Array<'_>>(b"CS"))
                        .expect("the copy is drawn with /CS");
                    let palette = match space.items::<Object<'_>>().nth(3).flatten() {
                        Some(Object::String(palette)) => palette.to_vec(),
                        other => panic!("no palette: {other:?}"),
                    };
                    assert_eq!(
                        palette,
                        rc4::PALETTE,
                        "whole: {whole}, pages {pages:?}, query {kind}, page {page}"
                    );
                }
            }
        }
    }
}

/// A file of one 200 × 200 pt page under a node with `node` entries, the page with
/// `page` entries besides its type, parent, box and content, and object 5 `fifth`, a
/// font as object 6, and as object 7 an appearance stream with `appearance` entries.
fn drawn_with(node: &str, page: &str, fifth: &str, appearance: &str) -> Pdf {
    Pdf::new(fixtures::file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            format!("<< /Type /Pages /Kids [3 0 R] /Count 1 {node} >>").into_bytes(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R {page} >>"
            )
            .into_bytes(),
            fixtures::stream("", b"BT /F1 12 Tf 10 10 Td (HELLO) Tj ET"),
            fifth.as_bytes().to_vec(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            fixtures::stream(&format!("/BBox [0 0 50 50] {appearance}"), b"0 0 5 5 re f"),
        ],
        "",
    ))
    .unwrap()
}

// A page drawn with an object the file lists but that cannot be read, its resources,
// a category of them, resources or a box it inherits, fails the extraction rather
// than giving a copy without what it lost; one the page tree cannot read, as its own
// dictionary, may have held pages, and fails it too. So does an appearance whose own
// resources cannot be read. An object the file does not hold, or one the page is not
// drawn with, as /Thumb, loses nothing.
#[test]
fn a_page_drawn_with_an_object_that_cannot_be_read_is_an_error() {
    let fonts = "<< /Font << /F1 6 0 R >> /Broken >";
    let lost = |number| ObjRef::new(number, 0);
    for (node, page, fifth, object) in [
        ("", "/Resources 5 0 R", fonts, 5),
        (
            "",
            "/Resources << /Font 5 0 R >>",
            "<< /F1 6 0 R /Broken >",
            5,
        ),
        ("/Resources 5 0 R", "", fonts, 5),
        ("/CropBox 5 0 R", "", "[0 0 200 200", 5),
    ] {
        let source = drawn_with(node, page, fifth, "");
        for query in page_queries(0, [200.0, 200.0]) {
            let result = run(&source, &[query]);
            assert!(
                matches!(result, Err(ExtractionError::UnreadablePage(0, id)) if id == lost(object)),
                "{node} {page} {fifth}: {:?}",
                result.map(|r| r.root_refs)
            );
        }
        // An appearance drawn with the page's resources, as it has none.
        let result = run(&source, &[ExtractionQuery::new_form(lost(7), 0, AT)]);
        assert!(
            matches!(result, Err(ExtractionError::UnreadablePage(0, id)) if id == lost(object)),
            "{node} {page} {fifth}: {:?}",
            result.map(|r| r.root_refs)
        );
    }
    let source = drawn_with("", "/Broken >", "null", "");
    for query in page_queries(0, [200.0, 200.0]) {
        let result = run(&source, &[query]);
        assert!(
            matches!(result, Err(ExtractionError::UnreadablePageTree(id)) if id == lost(3)),
            "{:?}",
            result.map(|r| r.root_refs)
        );
    }
    let source = drawn_with(
        "",
        "/Resources << /Font << /F1 6 0 R >> >>",
        fonts,
        "/Resources 5 0 R",
    );
    let result = run(&source, &[ExtractionQuery::new_form(lost(7), 0, AT)]);
    assert!(
        matches!(result, Err(ExtractionError::UnreadablePage(0, id)) if id == lost(5)),
        "{:?}",
        result.map(|r| r.root_refs)
    );
    for (node, page) in [
        ("", "/Resources 9 0 R"),
        ("/Resources 9 0 R", ""),
        ("", "/Resources << /Font << /F1 6 0 R >> >> /Thumb 5 0 R"),
    ] {
        let source = drawn_with(node, page, "<< /Broken >", "");
        for query in page_queries(0, [200.0, 200.0]) {
            let result = run(&source, &[query]).unwrap();
            assert!(
                result.root_refs[0].is_ok(),
                "{node} {page}: {:?}",
                result.root_refs[0]
            );
        }
    }
}

// A page whose dictionary holds a name with a `#` that is no escape, a name as
// `MuPDF` reads it (`PyMuPDF` 1.28.2), is read and copied whole, its box and its
// drawing: the file's table, which is sound, is checked by reading each object as
// reading it does.
#[test]
fn a_page_holding_a_name_that_is_no_key_is_copied_whole() {
    const DRAWING: &[u8] = b"0 0 1 rg 0 0 50 50 re f";
    let source = Pdf::new(fixtures::file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R /Broken#zz 5 >>"
                .to_vec(),
            fixtures::stream("", DRAWING),
        ],
        "",
    ))
    .unwrap();
    let media_box = source.pages()[0].media_box();
    assert_eq!(
        [media_box.x0, media_box.y0, media_box.x1, media_box.y1],
        [0.0, 0.0, 600.0, 700.0]
    );
    for (kind, query) in page_queries(0, [600.0, 700.0]).into_iter().enumerate() {
        let result = run(&source, &[query]).unwrap();
        let copy = reread(&result);
        let root = result.root_refs[0].unwrap();
        let (bounds, drawing) = if kind == 0 {
            let page = copied_dict(&copy, root);
            let contents = page.get::<Stream<'_>>(b"Contents").expect("contents");
            (
                page.get::<Rect>(b"MediaBox"),
                contents.decoded().unwrap().to_vec(),
            )
        } else {
            let form = get(&copy, root);
            (
                form.dict().get::<Rect>(b"BBox"),
                form.decoded().unwrap().to_vec(),
            )
        };
        let bounds = bounds.expect("a box");
        assert_eq!(
            [bounds.x0, bounds.y0, bounds.x1, bounds.y1],
            [0.0, 0.0, 600.0, 700.0],
            "query {kind}"
        );
        assert_eq!(drawing, DRAWING, "query {kind}");
    }
}

/// Whether copying page `page` of `source` with each kind of query gives a whole
/// copy (`true`) or an error (`false`); the one error the queries meet is returned.
fn copied_whole(source: &Pdf, page: usize) -> Result<(), ExtractionError> {
    for query in page_queries(page, [200.0, 200.0]) {
        let result = run(source, &[query])?;
        if let Some(Err(error)) = result.root_refs.into_iter().next() {
            return Err(error);
        }
    }
    Ok(())
}

/// The objects of a file of one page drawn with each kind of object a copy reads: a
/// node it inherits a box from, a box holding a referred number, a rotation, two
/// content streams, resources, a category of fonts, a font, an image with a color
/// space; and with objects no copy is drawn with: an image's and the catalog's
/// metadata, a thumbnail and private data. Object `n` is at index `n - 1`.
const GRAPH: [&str; 17] = [
    "<< /Type /Catalog /Pages 2 0 R /Metadata 17 0 R >>",
    "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox 8 0 R >>",
    "<< /Type /Page /Parent 2 0 R /Contents [4 0 R 5 0 R] /Resources 6 0 R \
     /CropBox [0 0 9 0 R 200] /Rotate 10 0 R /Thumb 15 0 R /PieceInfo 16 0 R >>",
    "STREAM BT /F1 12 Tf 10 10 Td (HI) Tj ET",
    "STREAM q 20 0 0 20 50 50 cm /Im Do Q",
    "<< /Font 7 0 R /XObject << /Im 11 0 R >> >>",
    "<< /F1 12 0 R >>",
    "[0 0 200 200]",
    "200",
    "0",
    "STREAM /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace 13 0 R \
     /BitsPerComponent 8 /Metadata 14 0 R|x",
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    "/DeviceGray",
    "STREAM /Type /Metadata /Subtype /XML|<x/>",
    "STREAM /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8|x",
    "<< /App << /Private 1 >> >>",
    "STREAM /Type /Metadata /Subtype /XML|<x/>",
];

/// The objects of [`GRAPH`] that no copy of its page is drawn with.
const NOT_DRAWN_WITH: [u32; 4] = [14, 15, 16, 17];

/// [`GRAPH`] with object `broken` replaced by what `broken_as` makes of it.
fn graph(broken: u32, broken_as: impl Fn(&str) -> Vec<u8>) -> Pdf {
    let objects: Vec<Vec<u8>> = GRAPH
        .iter()
        .zip(1..)
        .map(|(object, number)| {
            if number == broken {
                return broken_as(object);
            }
            match object.strip_prefix("STREAM ") {
                Some(stream) => match stream.split_once('|') {
                    Some((dict, data)) => fixtures::stream(dict, data.as_bytes()),
                    None => fixtures::stream("", stream.as_bytes()),
                },
                None => object.as_bytes().to_vec(),
            }
        })
        .collect();
    Pdf::new(fixtures::file(&objects, "")).unwrap()
}

// One object a page's copy is drawn with that the file lists but that cannot be read,
// wherever it is in what the copy reads, fails the copy; one it is not drawn with,
// metadata, a thumbnail, private data, does not. So does a stream whose dictionary
// can be read but whose data cannot.
#[test]
fn an_object_that_cannot_be_read_fails_a_copy_exactly_where_it_is_drawn_with() {
    let whole = graph(0, |_| unreachable!());
    copied_whole(&whole, 0).expect("the whole file is copied");
    for broken in 2..=GRAPH.len() as u32 {
        let drawn_with = !NOT_DRAWN_WITH.contains(&broken);
        let source = graph(broken, |_| b"<< /Broken >".to_vec());
        let copied = copied_whole(&source, 0);
        assert_eq!(copied.is_ok(), !drawn_with, "object {broken}: {copied:?}");
        if GRAPH[broken as usize - 1].starts_with("STREAM ") {
            // Data in another file, which a reader cannot read.
            let source = graph(broken, |_| fixtures::stream("/F (elsewhere)", b"x"));
            let copied = copied_whole(&source, 0);
            assert_eq!(
                copied.is_ok(),
                !drawn_with,
                "data of object {broken}: {copied:?}"
            );
        }
    }
}

// What reading one page lost, read once for the pages that share it, is lost to each
// of them however often it is read: a box they share whose number cannot be read, a
// category of resources whose font cannot be, content whose stream cannot be.
#[test]
fn a_page_sharing_what_another_read_without_an_object_is_not_copied() {
    for (shared, fifth) in [
        ("/MediaBox 5 0 R", "[0 0 6 0 R 200]"),
        ("/MediaBox [0 0 6 0 R 200]", "null"),
        ("/Resources << /Font 5 0 R >>", "<< /F1 6 0 R >>"),
        ("/Contents [5 0 R 6 0 R]", "null"),
    ] {
        let source = Pdf::new(fixtures::file(
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 200 200] >>".to_vec(),
                format!("<< /Type /Page /Parent 2 0 R {shared} >>").into_bytes(),
                format!("<< /Type /Page /Parent 2 0 R {shared} >>").into_bytes(),
                fifth.as_bytes().to_vec(),
                b"<< /Broken >".to_vec(),
            ],
            "",
        ))
        .unwrap();
        for order in [[0, 1], [1, 0]] {
            for page in order {
                for _ in 0..2 {
                    let copied = copied_whole(&source, page);
                    assert!(copied.is_err(), "{shared}, page {page} of {order:?}");
                }
            }
        }
    }
}

// A page tree whose /Kids cannot be read may have held pages: copying any page fails,
// rather than a copy of the file without them. /Kids the file does not hold lose
// nothing.
#[test]
fn a_page_tree_whose_kids_cannot_be_read_fails_every_copy() {
    for (kids, whole) in [("[6 0 R", false), ("[6 0 R]", true), ("null", true)] {
        let source = Pdf::new(fixtures::file(
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R 4 0 R] /MediaBox [0 0 200 200] >>".to_vec(),
                b"<< /Type /Pages /Parent 2 0 R /Kids 5 0 R >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R >>".to_vec(),
                kids.as_bytes().to_vec(),
                b"<< /Type /Page /Parent 3 0 R >>".to_vec(),
            ],
            "",
        ))
        .unwrap();
        let pages = source.pages().len();
        let copied = copied_whole(&source, 0);
        if whole {
            assert_eq!(pages, if kids == "null" { 1 } else { 2 }, "{kids}");
            copied.expect(kids);
        } else {
            assert!(
                matches!(copied, Err(ExtractionError::UnreadablePageTree(id)) if id == ObjRef::new(5, 0)),
                "{kids}: {copied:?}"
            );
        }
    }
}

// A category of resources whose dictionary can be read, written as a stream whose data
// cannot be, is the dictionary: its font is copied, and the page is drawn with it.
#[test]
fn a_category_whose_stream_data_cannot_be_read_is_copied_as_its_dictionary() {
    // Last, so that no stream's end after it is taken for its own.
    let source = Pdf::new(fixtures::file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
              /Resources << /Font 6 0 R >> >>"
                .to_vec(),
            fixtures::stream("", b"BT /F1 12 Tf 10 10 Td (HELLO) Tj ET"),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
            b"<< /F1 5 0 R /Length 9999 >>\nstream\nshort of its length".to_vec(),
        ],
        "",
    ))
    .unwrap();
    for query in page_queries(0, [200.0, 200.0]) {
        let result = run(&source, &[query]).unwrap();
        let copy = reread(&result);
        let font = copied_dict(&copy, result.root_refs[0].unwrap())
            .get::<Dict<'_>>(b"Resources")
            .and_then(|r| r.get::<Dict<'_>>(b"Font"))
            .and_then(|f| f.get::<Dict<'_>>(b"F1"))
            .expect("the font is copied");
        assert_eq!(
            font.get::<Name<'_>>(b"BaseFont").as_deref(),
            Some(b"Helvetica".as_slice())
        );
    }
}

/// `value`, read from `pdf`, shown with each reference as what it leads to, `null`
/// where it leads to nothing: what a reader makes of it.
fn shown(pdf: &Pdf, value: &Written<'_>) -> String {
    match value {
        Written::Ref(r) => match pdf.xref().written((*r).into()) {
            None | Some(Written::Null) => "R(null)".into(),
            Some(target) => format!("R({})", shown(pdf, &target)),
        },
        Written::Null => "null".into(),
        Written::Boolean(b) => format!("{b}"),
        Written::Number(token) => format!("N({})", String::from_utf8_lossy(token)),
        Written::String(s) => format!("S{:?}", s.as_bytes()),
        Written::Name(n) => format!("/{:?}", n.as_ref()),
        Written::Dict(d) => {
            let entries: Vec<String> = d
                .written()
                .map(|(k, v)| format!("{:?}:{}", k.as_ref(), shown(pdf, &v)))
                .collect();
            format!("<<{}>>", entries.join(","))
        }
        Written::Array(a) => {
            let items: Vec<String> = a.written().map(|v| shown(pdf, &v)).collect();
            format!("[{}]", items.join(","))
        }
        Written::Stream(_) => "stream".into(),
    }
}

/// The values of a resource form's dictionary, as `PyMuPDF` 1.28.2 reads them in
/// the source (`Document::xref_get_key`): numbers whose tokens it reads otherwise
/// than their values say (`4294967297` as 1, `--5` as -5, `5.0` as a real,
/// `4294967297.5` as 1.5), names whose `#` is no escape, strings with octal
/// escapes beyond a byte and line ends, and references to no object, direct, in
/// arrays and dictionaries, and as a key's value, which `MuPDF` tells from `null`.
const WRITTEN: &[&str] = &[
    "4294967297",
    "--5",
    "5.0",
    "612.0",
    ".5",
    "3-",
    "1.-5",
    "-0.0",
    "0000000001.5",
    "4294967297.5",
    "99999999999999999999",
    "2147483648",
    "/A#2",
    "/A#G1",
    "/A##41",
    "/A#20B",
    "/A#00",
    "(\\777)",
    "(a\r\nb)",
    "(a\\\n\nb)",
    "<414>",
    "true",
    "foo",
    "[1 2.5 /N (s) [null true] << /K 99 0 R >> 99 0 R]",
    "<< /A 99 0 R /B 5 0 R >>",
    "99 0 R",
    "5 0 R",
];

/// Every value a copy writes is read as the source's is: a number as the bytes it
/// is written as, a name or string as the bytes it is read as, a reference to no
/// object as a reference to an object that is `null`, never a direct `null`.
#[test]
fn values_are_copied_as_the_file_writes_them() {
    let entries: String = WRITTEN
        .iter()
        .enumerate()
        .map(|(i, v)| format!("/K{i} {v} "))
        .collect();
    let source = Pdf::new(fixtures::one_page(
        200,
        "/Resources << /XObject << /X 4 0 R >> >> /Contents 6 0 R",
        vec![
            fixtures::stream(
                &format!("/Type /XObject /Subtype /Form /BBox [0 0 9 9] {entries}"),
                b"",
            ),
            b"4294967297".to_vec(),
            fixtures::stream("", b"/X Do"),
        ],
    ))
    .unwrap();
    let form = source
        .xref()
        .get::<Stream<'_>>(ObjRef::new(4, 0).into())
        .unwrap();
    for query in page_queries(0, [200.0, 200.0]) {
        let result = run(&source, &[query]).unwrap();
        let copy = reread(&result);
        let copied = (1..100)
            .filter_map(|n| copy.xref().get::<Stream<'_>>(ObjRef::new(n, 0).into()))
            .find(|s| s.dict().contains_key(b"K0"))
            .expect("the form's copy");
        for (i, value) in WRITTEN.iter().enumerate() {
            let key = format!("K{i}");
            let read = |pdf: &Pdf, stream: &Stream<'_>| {
                shown(pdf, &stream.dict().get_written(key.as_bytes()).unwrap())
            };
            assert_eq!(read(&copy, &copied), read(&source, &form), "{value:?}");
        }
        let text = String::from_utf8_lossy(result.chunk.as_bytes()).into_owned();
        for token in ["/K0 4294967297", "/K1 --5", "/K2 5.0", "/K9 4294967297.5"] {
            assert!(text.contains(token), "{token} in {text}");
        }
        assert!(
            !text.contains("null>>"),
            "a reference written as null: {text}"
        );
    }
}

/// A string of an encrypted file is copied as it decrypts.
#[test]
fn strings_of_an_encrypted_file_are_copied_decrypted() {
    let source = Pdf::new(rc4::file(
        &[
            rc4::Part::Object("<< /Type /Catalog /Pages 2 0 R >>", b""),
            rc4::Part::Object("<< /Type /Pages /Kids [3 0 R] /Count 1 >>", b""),
            rc4::Part::Object(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9 9] \
                 /Resources << /ExtGState << /G 4 0 R >> >> >>",
                b"",
            ),
            rc4::Part::Object(
                "<< /Type /ExtGState /Note STRING /N [STRING] >>",
                b"a\r\n)b\xff",
            ),
        ],
        &[],
    ))
    .unwrap();
    let result = run(&source, &[ExtractionQuery::new_page(0)]).unwrap();
    let copy = reread(&result);
    let state = (1..20)
        .filter_map(|n| copy.xref().get::<Dict<'_>>(ObjRef::new(n, 0).into()))
        .find(|d| d.contains_key(b"Note"))
        .expect("the state's copy");
    let note = state.get::<hayro_syntax::object::String<'_>>(b"Note");
    assert_eq!(note.unwrap().as_bytes(), b"a\r\n)b\xff");
    let listed = state.get::<Array<'_>>(b"N").unwrap();
    let item = listed.iter::<hayro_syntax::object::String<'_>>().next();
    assert_eq!(item.unwrap().as_bytes(), b"a\r\n)b\xff");
}
