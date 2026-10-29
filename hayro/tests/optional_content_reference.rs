//! Content marked as optional content is drawn as `PDFium` draws it, on the cases
//! `optional_content/generate.py` writes with `PDFium`'s verdicts: groups typed or
//! not, of every intent and usage, listed or not, by an entry of their own or
//! another's, under every configuration; membership dictionaries by reference or in
//! place, with every policy, list and visibility expression, well formed or not; on
//! named `BDC` property lists, forms and images; values that refer to nothing or to
//! an object read in another role, and groups written in place in lists that the
//! catalog and membership dictionaries share. A page copied by `hayro-write` with
//! the catalog's `/OCProperties` draws as its source does.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::hayro_syntax::object::dict::keys::OCPROPERTIES;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use hayro_write::pdf_writer::{self, Content, Finish, Name, Ref};
use hayro_write::{ChunkSettings, ExtractionError, ExtractionQuery, extract};

/// Each case: its number, whether `PDFium` draws it, and the file.
fn cases() -> Vec<(usize, bool, Vec<u8>)> {
    let text = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/optional_content/cases.txt"
    ))
    .expect("the cases");
    let mut cases = Vec::new();
    let mut at = text.iter().position(|b| *b == b'\n').expect("a header") + 1;
    while at < text.len() {
        let end = at + text[at..].iter().position(|b| *b == b'\n').expect("a line");
        let line = std::str::from_utf8(&text[at..end]).expect("a case line");
        let [_, n, drawn, len] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("{line}")
        };
        let len: usize = len.parse().expect("a length");
        let pdf = text[end + 1..end + 1 + len].to_vec();
        cases.push((n.parse().expect("a number"), drawn == "1", pdf));
        at = end + 1 + len + 1;
    }
    cases
}

/// Whether the centre of the first page of `pdf` is drawn green.
fn drawn(pdf: Vec<u8>) -> bool {
    let pdf = Pdf::new(pdf).expect("valid pdf");
    let pixmap = render(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: 1.0,
            y_scale: 1.0,
            bg_color: WHITE,
        },
    );
    let width = pixmap.width() as usize;
    let at = (25 * width + 25) * 4;
    let pixel = &pixmap.data_as_u8_slice()[at..at + 4];
    pixel[1] > 200 && pixel[0] < 50
}

#[test]
fn optional_content_is_drawn_as_pdfium_draws_it() {
    let cases = cases();
    assert!(cases.len() > 1000);
    let differing: Vec<(usize, bool)> = cases
        .into_iter()
        .filter_map(|(n, expected, pdf)| (drawn(pdf) != expected).then_some((n, expected)))
        .collect();
    assert!(
        differing.is_empty(),
        "{} cases differ from PDFium (case, PDFium draws it): {differing:?}",
        differing.len()
    );
}

/// The first page of `source` copied as a form, with the catalog's
/// `/OCProperties`, as the one page of a file; `None` where the extraction refuses
/// a page that refers to a stream whose data cannot be read.
fn copied(source: Vec<u8>) -> Option<Vec<u8>> {
    let source = Pdf::new(source).expect("valid pdf");
    let mut next = Ref::new(1);
    let (catalog, tree) = (next.bump(), next.bump());
    let queries = [
        ExtractionQuery::new_user_space_xobject(0, [0.0, 0.0, 50.0, 50.0]),
        ExtractionQuery::new_catalog_entry(OCPROPERTIES),
    ];
    let extracted = match extract(
        &source,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        &queries,
    ) {
        Ok(extracted) => extracted,
        Err(ExtractionError::UnreadablePage(..)) => return None,
        Err(other) => panic!("extraction: {other:?}"),
    };
    let [page_form, properties] = [0, 1].map(|i| extracted.root_refs[i].expect("copied"));
    let (page, content) = (next.bump(), next.bump());
    let mut out = pdf_writer::Pdf::new();
    out.extend(&extracted.chunk);
    out.catalog(catalog)
        .pages(tree)
        .pair(Name(b"OCProperties"), properties);
    out.pages(tree).kids([page]).count(1);
    let mut p = out.page(page);
    p.media_box(pdf_writer::Rect::new(0.0, 0.0, 50.0, 50.0))
        .parent(tree)
        .contents(content);
    p.resources().x_objects().pair(Name(b"P"), page_form);
    p.finish();
    let mut c = Content::new();
    c.x_object(Name(b"P"));
    out.stream(content, &c.finish());
    Some(out.finish())
}

/// A page whose content refers to a stream with data kept in another file is
/// refused, as no copy holds what it reads; only such pages are.
#[test]
fn a_copied_page_draws_its_optional_content_as_the_source() {
    let mut refused = Vec::new();
    let differing: Vec<(usize, bool)> = cases()
        .into_iter()
        .filter_map(|(n, expected, pdf)| {
            let elsewhere = pdf
                .windows(13)
                .any(|w| w == b"/F (elsewhere)"[..13].as_ref());
            match copied(pdf) {
                Some(copy) => (drawn(copy) != expected).then_some((n, expected)),
                None => {
                    assert!(elsewhere, "case {n} refused");
                    refused.push(n);
                    None
                }
            }
        })
        .collect();
    assert!(refused.len() < 100, "{} refused", refused.len());
    assert!(
        differing.is_empty(),
        "{} copies differ from their source (case, the source drawn): {differing:?}",
        differing.len()
    );
}
