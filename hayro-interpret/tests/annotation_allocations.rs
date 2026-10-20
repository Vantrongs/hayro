//! What generating the appearance of one more form field allocates stays under a
//! ceiling: a form of hundreds of fields, each drawn by the generator, makes that
//! many times the requests to the allocator one field makes.

use hayro_interpret::AnnotationSelection;
use hayro_interpret::InterpreterSettings;
use hayro_interpret::annotation::{Appearances, View};
use hayro_interpret::hayro_syntax::Pdf;

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
#[path = "../../hayro-syntax/tests/common/ceilings.rs"]
mod ceilings;
mod common;

/// A page 600 pt square.
const PAGE: kurbo::Rect = kurbo::Rect::new(0.0, 0.0, 600.0, 600.0);

/// A file whose one page holds `n` empty text fields and `n` check boxes, each with
/// a border, as the fields of a form are.
fn form(n: usize) -> Pdf {
    let mut annots = vec![];
    let mut objects = vec![];
    for i in 0..n {
        let (x, y) = (10 + (i % 20) * 28, 10 + (i / 20) * 40);
        annots.push(format!("{} 0 R {} 0 R", 4 + 2 * i, 5 + 2 * i));
        objects.push(
            format!(
                "<< /Type /Annot /Subtype /Widget /FT /Tx /T (t{i}) /DA (/Helv 10 Tf 0 g) \
                 /MK << /BC [0] >> /F 4 /Rect [{x} {y} {} {}] >>",
                x + 20,
                y + 15
            )
            .into_bytes(),
        );
        objects.push(
            format!(
                "<< /Type /Annot /Subtype /Widget /FT /Btn /T (b{i}) /AS /Off \
                 /MK << /BC [0] /CA (4) >> /DA (/ZaDb 0 Tf 0 g) /F 4 \
                 /Rect [{x} {} {} {}] >>",
                y + 20,
                x + 12,
                y + 32
            )
            .into_bytes(),
        );
    }
    let file = common::one_page(
        600,
        &format!(
            "/Annots [{}] /Resources << /Font << /Helv << /Type /Font /Subtype /Type1 \
             /BaseFont /Helvetica >> >> >>",
            annots.join(" ")
        ),
        objects,
    );
    Pdf::new(file).expect("valid pdf")
}

/// The requests and bytes generating the appearances of the fields of `form(n)`
/// allocates.
fn cost(n: usize) -> (usize, usize) {
    let pdf = form(n);
    let (drawn, used) = allocations::measure(|| {
        let appearances =
            Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
        appearances
            .page(
                &pdf.pages()[0],
                AnnotationSelection::View,
                &View::new(PAGE, 1.0),
            )
            .len()
    });
    assert_eq!(drawn, 2 * n);
    (used.count, used.allocated)
}

/// One more text field and check box, as the difference between forms of 40 and
/// 20 of each, allocates no more than its ceilings.
#[test]
fn a_form_field_allocates_under_its_ceilings() {
    // What a thread allocates once, before the first form is measured, is not counted.
    cost(1);
    let (count_20, bytes_20) = cost(20);
    let (count_40, bytes_40) = cost(40);
    ceilings::check_ceilings(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/ceilings/annotation_allocations.txt"
        ),
        "WRITE_CEILINGS=1 cargo test -p hayro-interpret --test annotation_allocations",
        &[
            ("field_pair_allocations", (count_40 - count_20) / 20),
            ("field_pair_bytes", (bytes_40 - bytes_20) / 20),
        ],
    );
}
