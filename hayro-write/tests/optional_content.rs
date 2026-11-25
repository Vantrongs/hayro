//! Optional content in extracted pages and forms.

use hayro_syntax::Pdf;
use hayro_syntax::object::{Array, Dict, Name as PdfName, Stream};
use hayro_write::{extract_pages_as_xobject_to_pdf, extract_pages_to_pdf};
use pdf_writer::{Finish, Name, Rect, Ref, Str};

fn source(indirect: bool) -> Vec<u8> {
    let mut pdf = pdf_writer::Pdf::new();
    let [catalog, tree, a, b, group, form, content, config] =
        [1, 2, 3, 4, 5, 6, 7, 8].map(Ref::new);
    let write_properties = |mut props: pdf_writer::Dict<'_>| {
        props.insert(Name(b"OCGs")).array().item(group);
        props
            .insert(Name(b"D"))
            .dict()
            .insert(Name(b"OFF"))
            .array()
            .item(group);
    };
    let mut cat = pdf.catalog(catalog);
    cat.pages(tree);
    if indirect {
        cat.pair(Name(b"OCProperties"), config);
        cat.finish();
        write_properties(pdf.indirect(config).dict());
    } else {
        write_properties(cat.insert(Name(b"OCProperties")).dict());
        cat.finish();
    }
    pdf.pages(tree).kids([a, b]).count(2);
    let bbox = Rect::new(0., 0., 100., 100.);
    for id in [a, b] {
        let mut page = pdf.page(id);
        page.parent(tree).media_box(bbox).contents(content);
        let mut resources = page.resources();
        resources.x_objects().pair(Name(b"Fm"), form);
        resources
            .insert(Name(b"Properties"))
            .dict()
            .pair(Name(b"oc1"), group);
    }
    pdf.indirect(group)
        .dict()
        .pair(Name(b"Type"), Name(b"OCG"))
        .pair(Name(b"Name"), Str(b"Hidden"));
    pdf.form_xobject(form, b"50 50 30 30 re f")
        .bbox(bbox)
        .pair(Name(b"OC"), group);
    pdf.stream(content, b"/OC /oc1 BDC 10 10 20 20 re f EMC /Fm Do");
    pdf.finish()
}

fn catalog(pdf: &Pdf) -> Dict<'_> {
    pdf.xref().get(pdf.xref().root_id()).unwrap()
}

#[test]
fn shared_layer_in_pages_and_forms() {
    for indirect in [false, true] {
        let src = Pdf::new(source(indirect)).unwrap();
        for as_form in [false, true] {
            let copy = if as_form {
                extract_pages_as_xobject_to_pdf
            } else {
                extract_pages_to_pdf
            };
            let pdf = Pdf::new(copy(&src, &[0, 1])).unwrap();
            let props = catalog(&pdf).get::<Dict<'_>>("OCProperties").unwrap();
            let group = props
                .get::<Array<'_>>("OCGs")
                .unwrap()
                .raw_iter()
                .next()
                .unwrap()
                .as_obj_ref()
                .unwrap();
            let off = props
                .get::<Dict<'_>>("D")
                .unwrap()
                .get::<Array<'_>>("OFF")
                .unwrap();
            assert_eq!(off.raw_iter().next().unwrap().as_obj_ref(), Some(group));
            let layer = pdf.xref().get::<Dict<'_>>(group.into()).unwrap();
            assert_eq!(layer.get::<PdfName<'_>>("Type").unwrap().as_ref(), b"OCG");
            assert_eq!(pdf.pages().len(), 2);
            for page in pdf.pages().iter() {
                let container = if as_form {
                    page.resources()
                        .x_objects
                        .get::<Stream<'_>>("O1")
                        .unwrap()
                        .dict()
                        .clone()
                } else {
                    page.raw().clone()
                };
                let resources = container.get::<Dict<'_>>("Resources").unwrap();
                assert_eq!(
                    resources
                        .get::<Dict<'_>>("Properties")
                        .unwrap()
                        .get_ref("oc1"),
                    Some(group)
                );
                let form = resources
                    .get::<Dict<'_>>("XObject")
                    .unwrap()
                    .get::<Stream<'_>>("Fm")
                    .unwrap();
                assert_eq!(form.dict().get_ref("OC"), Some(group));
            }
        }
    }
}

#[test]
fn no_layers_in_plain_pdf() {
    let mut plain = pdf_writer::Pdf::new();
    let [root, tree, page] = [1, 2, 3].map(Ref::new);
    plain.catalog(root).pages(tree);
    plain.pages(tree).kids([page]).count(1);
    plain
        .page(page)
        .parent(tree)
        .media_box(Rect::new(0., 0., 100., 100.))
        .resources();
    let src = Pdf::new(plain.finish()).unwrap();
    for copy in [extract_pages_to_pdf, extract_pages_as_xobject_to_pdf] {
        let pdf = Pdf::new(copy(&src, &[0])).unwrap();
        assert!(!catalog(&pdf).contains_key("OCProperties"));
    }
}

#[test]
fn layer_properties_in_recovered_catalog() {
    let mut bytes = source(false);
    let end = bytes.windows(5).position(|w| w == b"xref\n").unwrap();
    bytes.truncate(end);
    let src = Pdf::new(bytes).unwrap();
    let pdf = Pdf::new(extract_pages_to_pdf(&src, &[0])).unwrap();
    assert!(catalog(&pdf).get::<Dict<'_>>("OCProperties").is_some());
}
