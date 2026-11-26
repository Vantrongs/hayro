//! Numeric readings used to select and draw annotations.

use hayro_annotations::{Appearances, Look, Unsupported, View};
use hayro_interpret::{AnnotationSelection, InterpreterSettings};

mod common;

fn looks(entries: &str) -> Vec<Result<f32, Unsupported>> {
    let pdf = common::document("/Annots [4 0 R]", vec![format!("<< {entries} >>")]);
    let page = &pdf.pages()[0];
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    appearances
        .page(page, AnnotationSelection::Flatten, &View::of(page))
        .iter()
        .map(|drawn| match drawn.look() {
            Look::Generated(appearance) => Ok(appearance.opacity),
            Look::Unsupported(why) => Err(*why),
            _ => panic!("expected a generated appearance or refusal"),
        })
        .collect()
}

#[test]
fn annotation_flags_and_opacity_keep_their_written_digits() {
    let square = "/Subtype /Square /Rect [10 10 100 80] /IC [1 0 0]";
    assert_eq!(
        looks(&format!(
            "{square} /F 4294967296.4999999 /CA 4503599627370496.25"
        )),
        [Ok(0.25)]
    );
    assert!(looks(&format!("{square} /F 4294967297.5")).is_empty());
    for key in ["F", "CA"] {
        assert_eq!(
            looks(&format!("{square} /{key} 9223372036854775808")),
            [Err(Unsupported::Number)]
        );
    }
}

#[test]
fn invalid_shared_appearance_operands_remain_invalid_on_replay() {
    let annot = "<< /Subtype /FreeText /Rect [10 10 100 80] /Contents (abc) /DA 6 0 R >>";
    for da in [
        "(/Helv 4294967308.5 Tf 0 g)",
        "(/Helv 12 Tf 4503599627370496.25 g)",
    ] {
        let pdf = common::document(
            "/Annots [4 0 R 5 0 R]",
            vec![annot.into(), annot.into(), da.into()],
        );
        let page = &pdf.pages()[0];
        let app = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
        let drawn = app.page(page, AnnotationSelection::View, &View::of(page));
        assert_eq!(drawn.len(), 2);
        assert!(
            drawn
                .iter()
                .all(|d| matches!(d.look(), Look::Unsupported(Unsupported::Number)))
        );
    }
}

#[test]
fn unreadable_alignment_is_refused_where_the_text_may_show() {
    let text = "/Subtype /FreeText /DA (/Helv 12 Tf 0 g) /Contents (abc) /Q 9223372036854775808";
    assert_eq!(
        looks(&format!("{text} /Rect [10 10 100 80]")),
        [Err(Unsupported::Number)]
    );
    assert!(looks(&format!("{text} /Rect [-100 -100 -20 -20]")).is_empty());
}
