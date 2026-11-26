//! Pixel parity for direct and serialized annotation appearances.
use hayro::hayro_interpret::{AnnotationSelection, BlendMode, InterpreterSettings};
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::Pixmap;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use hayro_annotations::{Appearances, Look, View};
use std::fmt::Write;

fn stream(dict: &str, bytes: &str) -> String {
    format!(
        "<< {dict} /Length {} >>\nstream\n{bytes}\nendstream",
        bytes.len()
    )
}

#[path = "../../hayro-annotations/tests/common/mod.rs"]
mod common;
use common::document;

fn raster(pdf: &Pdf, settings: &InterpreterSettings) -> Pixmap {
    render(
        &pdf.pages()[0],
        &RenderCache::new(),
        settings,
        &RenderSettings::default(),
        &PixmapSettings::default(),
    )
}

fn compare(dict: &str, text: bool, expected_blend: BlendMode) {
    let background = "0.2 0.6 0.9 rg 0 0 128 96 re f\n";
    let source = document(
        "/Annots [5 0 R]",
        vec![stream("", background), dict.to_owned()],
    );
    let settings = InterpreterSettings::default();
    let page = &source.pages()[0];
    let generator = Appearances::new(source.xref(), settings.font_resolver.clone());
    let annots = generator.page(page, AnnotationSelection::View, &View::of(page));
    let Look::Generated(a) = annots[0].look() else {
        panic!("expected a generated appearance")
    };
    assert_eq!(a.blend, expected_blend);
    let content = a.content();
    if text {
        assert_eq!(a.glyphs.len(), 1);
        assert_eq!(content.matches(" Do\n").count(), 4);
    } else {
        assert_eq!(a.opacity, 0.5);
    }
    let blend = if a.blend == BlendMode::Multiply {
        "Multiply"
    } else {
        "Normal"
    };
    let mut objects = vec![
        stream("", &format!("{background}q /State gs\n{content}Q")),
        format!("<< /BM /{blend} /ca {} /CA {} >>", a.opacity, a.opacity),
    ];
    let mut names = String::new();
    for g in &a.glyphs {
        write!(names, "/{} {} 0 R ", g.name(), objects.len() + 4).unwrap();
        let [x, y, u, v] = g.bbox();
        objects.push(stream(
            &format!("/Type /XObject /Subtype /Form /BBox [{x} {y} {u} {v}]"),
            &g.content(),
        ));
    }
    let flattened = document(
        &format!("/Resources << /XObject << {names} >> /ExtGState << /State 5 0 R >> >>"),
        objects,
    );
    let direct = raster(&source, &settings);
    let serialized = raster(&flattened, &settings);
    let changed = direct
        .data()
        .iter()
        .filter(|p| **p != direct.data()[0])
        .count();
    assert!(changed > 50, "only {changed} painted pixels");
    let mismatches = direct
        .data()
        .iter()
        .zip(serialized.data())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(mismatches, 0);
}

#[test]
fn repeated_freetext_glyphs() {
    compare(
        "<< /Subtype /FreeText /Rect [10 15 116 80] /BS << /W 0 >> /DA (/Helv 16 Tf 0 g) /Contents (AAAA) >>",
        true,
        BlendMode::Normal,
    );
}

#[test]
fn overlapping_multiply_highlights() {
    compare(
        "<< /Subtype /Highlight /Rect [10 15 115 80] /C [1 0.7 0.2] /CA 0.5 /QuadPoints [10 56 90 56 10 30 90 30 45 68 115 68 45 42 115 42] >>",
        false,
        BlendMode::Multiply,
    );
}

#[test]
fn indirect_annotations_skip_a_reference_back_to_the_page() {
    let pdf = document(
        "/Annots 5 0 R /Rect [0 0 10 10] /AP << /N 6 0 R >>",
        vec![
            stream("", ""),
            "[3 0 R]".into(),
            stream("/Subtype /Form /BBox [0 0 10 10]", "0 0 10 10 re f"),
        ],
    );
    let page = &pdf.pages()[0];
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    assert!(
        appearances
            .page(page, AnnotationSelection::View, &View::of(page))
            .is_empty()
    );
}

#[test]
fn cached_annotation_selection_reports_the_unreadable_object() {
    let pdf = document("/Annots [5 0 R]", vec![stream("", ""), ")".into()]);
    let page = &pdf.pages()[0];
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    for _ in 0..2 {
        let (marks, lost) = pdf
            .xref()
            .watch(|| appearances.page(page, AnnotationSelection::View, &View::of(page)));
        assert!(marks.is_empty());
        assert_eq!(
            lost,
            Some(hayro::hayro_syntax::object::ObjectIdentifier::new(5, 0))
        );
    }
}

#[test]
fn translucent_bordered_square() {
    compare(
        "<< /Subtype /Square /Rect [10 15 90 80] /IC [0 1 0] /C [1 0 0] /BS << /W 6 >> /CA 0.5 >>",
        false,
        BlendMode::Normal,
    );
}
