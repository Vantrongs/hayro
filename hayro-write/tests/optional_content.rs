//! Copying a page whose content is in optional content groups, with the catalog's
//! `/OCProperties` that says which of them are on.

use hayro_write::hayro_syntax::Pdf;
use hayro_write::hayro_syntax::object::dict::keys::OCPROPERTIES;
use hayro_write::hayro_syntax::object::{Array, Dict, ObjRef, Stream};
use hayro_write::pdf_writer::{self, Content, Finish, Name, Ref};
use hayro_write::{ChunkSettings, ExtractionQuery, extract};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

/// A page that draws marked content in group 4, which the catalog turns off, and an
/// image form (5) in the same group by its own `/OC`.
fn layered() -> Pdf {
    let mut objects = fixtures::one_page_objects(
        100,
        "/Resources << /Properties << /oc1 4 0 R >> /XObject << /Fm 5 0 R >> >> /Contents 6 0 R",
    );
    objects[0] = b"<< /Type /Catalog /Pages 2 0 R \
        /OCProperties << /OCGs [4 0 R] /D << /OFF [4 0 R] /Order [4 0 R] >> >> >>"
        .to_vec();
    objects.push(b"<< /Type /OCG /Name (Hidden) >>".to_vec());
    objects.push(fixtures::stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 10 10] /OC 4 0 R",
        b"0 0 10 10 re f",
    ));
    objects.push(fixtures::stream(
        "",
        b"/OC /oc1 BDC 1 0 0 rg 0 0 50 50 re f EMC /Fm Do",
    ));
    Pdf::new(fixtures::file(&objects, "")).expect("valid pdf")
}

/// The page copied as a form and the catalog's `/OCProperties`, written as the one
/// page of a file and its catalog's.
fn copy(source: &Pdf) -> Pdf {
    let mut next = Ref::new(1);
    let (catalog, tree) = (next.bump(), next.bump());
    let queries = [
        ExtractionQuery::new_user_space_xobject(0, [0.0, 0.0, 100.0, 100.0]),
        ExtractionQuery::new_catalog_entry(OCPROPERTIES),
    ];
    let extracted = extract(
        source,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        &queries,
    )
    .expect("extraction");
    let [page_form, properties] = [0, 1].map(|i| extracted.root_refs[i].expect("copied"));
    let (page, content) = (next.bump(), next.bump());
    let mut out = pdf_writer::Pdf::new();
    out.extend(&extracted.chunk);
    out.catalog(catalog)
        .pages(tree)
        .pair(Name(b"OCProperties"), properties);
    out.pages(tree).kids([page]).count(1);
    let mut p = out.page(page);
    p.media_box(pdf_writer::Rect::new(0.0, 0.0, 100.0, 100.0))
        .parent(tree)
        .contents(content);
    p.resources().x_objects().pair(Name(b"P"), page_form);
    p.finish();
    let mut c = Content::new();
    c.x_object(Name(b"P"));
    out.stream(content, &c.finish());
    Pdf::new(out.finish()).expect("the copy parses")
}

fn first_ref(array: &Array<'_>) -> ObjRef {
    array
        .raw_iter()
        .next()
        .and_then(|o| o.as_obj_ref())
        .expect("a reference")
}

/// The groups the copied content names are the ones the copied `/OCProperties`
/// lists and turns off, so a viewer hides them as it hides them in the source;
/// without `/OCProperties`, or with groups copied apart, it shows them.
#[test]
fn the_copy_names_the_groups_its_ocproperties_turns_off() {
    let copy = copy(&layered());
    let xref = copy.xref();
    let catalog = xref.get::<Dict<'_>>(xref.root_id()).expect("catalog");
    let properties = catalog
        .get::<Dict<'_>>(OCPROPERTIES)
        .expect("the catalog has the copied /OCProperties");
    let listed = first_ref(&properties.get::<Array<'_>>(b"OCGs").expect("/OCGs"));
    let off = properties
        .get::<Dict<'_>>(b"D")
        .and_then(|d| d.get::<Array<'_>>(b"OFF"))
        .expect("/D /OFF");
    assert_eq!(first_ref(&off), listed);

    let page_form = copy.pages()[0]
        .resources()
        .x_objects
        .get::<Stream<'_>>("P")
        .expect("the page's copy");
    let resources = page_form
        .dict()
        .get::<Dict<'_>>(b"Resources")
        .expect("its resources");
    let marked = resources
        .get::<Dict<'_>>(b"Properties")
        .and_then(|p| p.get_ref(b"oc1"))
        .expect("/Properties /oc1");
    assert_eq!(marked, listed);
    let image_form = resources
        .get::<Dict<'_>>(b"XObject")
        .and_then(|x| x.get::<Stream<'_>>(b"Fm"))
        .expect("the form the page draws");
    assert_eq!(
        image_form.dict().get_ref(b"OC"),
        Some(listed),
        "the form keeps its /OC"
    );
}

/// A catalog without the entry copies as the object that is `null`.
#[test]
fn a_missing_catalog_entry_copies_as_null() {
    let source = Pdf::new(fixtures::one_page(100, "", Vec::new())).expect("valid pdf");
    let mut next = Ref::new(1);
    let extracted = extract(
        &source,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        &[ExtractionQuery::new_catalog_entry(OCPROPERTIES)],
    )
    .expect("extraction");
    let null = extracted.root_refs[0].expect("copied");
    let written = String::from_utf8_lossy(extracted.chunk.as_bytes()).into_owned();
    assert!(
        written.contains(&format!("{} 0 obj\nnull", null.get())),
        "{written}"
    );
}

/// Entries of the table that lead to one group's body are that group, as the
/// source is read: the copy writes it once, and every name and list refers to it.
#[test]
fn entries_leading_to_one_group_copy_as_that_group() {
    // A group needs no /Type where /OCProperties lists it.
    for group in ["<< /Type /OCG /Name (Layer) >>", "<< /Name (Layer) >>"] {
        let aliases = 200;
        // Object 4 is the group, 5 the page's content, 6, 7, … entries at its place.
        let listed: Vec<String> = std::iter::once(4)
            .chain(6..6 + aliases)
            .map(|n| format!("{n} 0 R"))
            .collect();
        let file = fixtures::file_with_aliases(
            vec![
                format!(
                    "<< /Type /Catalog /Pages 2 0 R \
                     /OCProperties << /OCGs [{}] /D << /OFF [6 0 R] >> >> >>",
                    listed.join(" ")
                )
                .into_bytes(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
                  /Resources << /Properties << /a 4 0 R /b 7 0 R >> >> /Contents 5 0 R >>"
                    .to_vec(),
                group.as_bytes().to_vec(),
                fixtures::stream(
                    "",
                    b"/OC /a BDC 0 0 50 50 re f EMC /OC /b BDC 50 0 50 50 re f EMC",
                ),
            ],
            &vec![4; aliases as usize],
        );
        let copy = copy(&Pdf::new(file).expect("valid pdf"));
        let xref = copy.xref();
        let properties = xref
            .get::<Dict<'_>>(xref.root_id())
            .and_then(|c| c.get::<Dict<'_>>(OCPROPERTIES))
            .expect("/OCProperties");
        let off = first_ref(
            &properties
                .get::<Dict<'_>>(b"D")
                .and_then(|d| d.get::<Array<'_>>(b"OFF"))
                .expect("/OFF"),
        );
        let listed = properties.get::<Array<'_>>(b"OCGs").expect("/OCGs");
        assert!(
            listed.raw_iter().all(|o| o.as_obj_ref() == Some(off)),
            "{group}"
        );
        let named = copy.pages()[0]
            .resources()
            .x_objects
            .get::<Stream<'_>>("P")
            .and_then(|p| p.dict().get::<Dict<'_>>(b"Resources"))
            .and_then(|r| r.get::<Dict<'_>>(b"Properties"))
            .expect("/Properties");
        assert_eq!(named.get_ref(b"a"), Some(off), "{group}");
        assert_eq!(named.get_ref(b"b"), Some(off), "{group}");
    }
}

/// An annotation's appearance copied as a form keeps none of its own `/OC`: `MuPDF`
/// draws an appearance whatever it says, and only the annotation's `/OC` hides it.
#[test]
fn an_appearance_copied_as_a_form_leaves_its_oc_out() {
    let mut objects = fixtures::one_page_objects(100, "");
    objects[0] = b"<< /Type /Catalog /Pages 2 0 R \
        /OCProperties << /OCGs [4 0 R] /D << /OFF [4 0 R] >> >> >>"
        .to_vec();
    objects.push(b"<< /Type /OCG /Name (Off) >>".to_vec());
    objects.push(fixtures::stream(
        "/BBox [0 0 10 10] /OC 4 0 R",
        b"0 0 10 10 re f",
    ));
    let source = Pdf::new(fixtures::file(&objects, "")).expect("valid pdf");
    let mut next = Ref::new(1);
    let at = hayro_write::FormPlacement {
        bbox: [0.0, 0.0, 10.0, 10.0],
        matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
    };
    let extracted = extract(
        &source,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        &[ExtractionQuery::new_form(ObjRef::new(5, 0), 0, at)],
    )
    .expect("extraction");
    let written = String::from_utf8_lossy(extracted.chunk.as_bytes()).into_owned();
    assert!(!written.contains("/OC"), "{written}");
}

/// Groups that refer to each other are copied one after another, however long the
/// chain: copying one does not wait on copying the next.
#[test]
fn a_long_chain_of_groups_copies() {
    let length = 50_000;
    let mut objects = fixtures::one_page_objects(100, "");
    objects[0] = b"<< /Type /Catalog /Pages 2 0 R \
        /OCProperties << /OCGs [4 0 R] /D << /OFF [4 0 R] >> >> >>"
        .to_vec();
    // Object 4 + i is group i, whose /Usage refers to group i + 1.
    for i in 0..length {
        objects.push(
            format!(
                "<< /Type /OCG /Name (G{i}) /Usage << /Next {} 0 R >> >>",
                5 + i
            )
            .into_bytes(),
        );
    }
    let source = Pdf::new(fixtures::file(&objects, "")).expect("valid pdf");
    let mut next = Ref::new(1);
    let extracted = extract(
        &source,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        &[ExtractionQuery::new_catalog_entry(OCPROPERTIES)],
    )
    .expect("extraction");
    let text = String::from_utf8_lossy(extracted.chunk.as_bytes());
    assert_eq!(text.matches("/Type /OCG").count(), length);
}
