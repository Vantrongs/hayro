//! A copy of the graphics state costs the same however much content set it: a Type 3
//! glyph takes a copy of the state, and so does each `q` that changes a parameter,
//! so a copy that grew with the dash array or with the glyph outlines a text object
//! gathered for clipping would make N glyphs or N `q`s cost N times that size.
//!
//! Each test measures what one more unit of work (a glyph, a `q … Q`, a glyph run)
//! allocates, as the difference between a page with 2N units and one with N, at a
//! size S of the content-sized part and at 2S, and requires the two to be equal: the
//! allocator is deterministic, so any growth with S shows as a difference.

use hayro_interpret::font::{Glyph, GlyphRun};
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings,
    SoftMask,
};
use kurbo::BezPath;

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
mod common;

/// Draws nothing but interprets the content of every Type 3 glyph it is given, as a
/// renderer does.
struct Interpreting;

impl<'a> Device<'a> for Interpreting {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        for glyph in run.glyphs() {
            if let Glyph::Type3(g) = &**glyph {
                g.interpret(
                    self,
                    glyph.transform(),
                    kurbo::Affine::IDENTITY,
                    &props.paint,
                );
            }
        }
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// A page with `content`, whose resources hold `/T3`, a Type 3 font whose glyph `A`
/// strokes a line in the state it inherits, and `/H`, Helvetica.
fn page(content: &str) -> Pdf {
    let objects = vec![
        common::stream("", content.as_bytes()),
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
           /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /a 6 0 R >> \
           /Encoding << /Differences [65 /a] >> /FirstChar 65 /LastChar 65 \
           /Widths [1000] /Resources << >> >>"
            .to_vec(),
        common::stream("", b"1000 0 d0 0 0 m 1000 1000 l S"),
    ];
    let file = common::one_page(
        200,
        "/Contents 4 0 R /Resources << /Font << /T3 5 0 R \
         /H << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >>",
        objects,
    );
    Pdf::new(file).expect("valid pdf")
}

/// The bytes and the requests interpreting the page of `content` allocates.
fn cost(content: &str) -> (usize, usize) {
    let pdf = page(content);
    let ((), used) = allocations::measure(|| {
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut Interpreting);
    });
    (used.allocated, used.count)
}

/// What `units` more units of `content(size, units)` allocate, in bytes and requests.
fn unit_cost(
    content: &impl Fn(usize, usize) -> String,
    size: usize,
    units: usize,
) -> (usize, usize) {
    let (bytes, count) = cost(&content(size, units));
    let (more_bytes, more_count) = cost(&content(size, 2 * units));
    (more_bytes - bytes, more_count - count)
}

/// Asserts that what a unit of `content(size, units)` allocates does not change when
/// `size` doubles.
fn assert_unit_cost_ignores_size(what: &str, content: impl Fn(usize, usize) -> String) {
    const SIZE: usize = 1000;
    const UNITS: usize = 100;
    // What a thread allocates once, before the first page is measured, is not counted.
    cost(&content(SIZE, UNITS));
    let small = unit_cost(&content, SIZE, UNITS);
    let large = unit_cost(&content, 2 * SIZE, UNITS);
    assert_eq!(
        small,
        large,
        "{what}: {UNITS} more units allocate (bytes, requests) {small:?} at size {SIZE} \
         and {large:?} at size {}",
        2 * SIZE
    );
}

/// A dash array of `n` entries.
fn dash(n: usize) -> String {
    format!("[{}] 0 d", "3 ".repeat(n))
}

#[test]
fn a_type3_glyph_costs_the_same_under_any_dash_array() {
    assert_unit_cost_ignores_size("Type 3 glyphs under a dash array", |m, n| {
        format!("{} BT /T3 1 Tf ({}) Tj ET", dash(m), "A".repeat(n))
    });
}

#[test]
fn a_tj_array_not_drawn_costs_the_same_per_glyph_under_any_dash_array() {
    assert_unit_cost_ignores_size("Type 3 glyphs of an array not drawn", |m, n| {
        format!("{} BT /T3 1 Tf [({}) null] TJ ET", dash(m), "A".repeat(n))
    });
}

#[test]
fn a_type3_glyph_costs_the_same_however_many_glyphs_clip() {
    assert_unit_cost_ignores_size("Type 3 glyphs after clipping glyphs", |k, n| {
        format!(
            "BT 7 Tr /H 1 Tf ({}) Tj /T3 1 Tf ({}) Tj ET",
            "B".repeat(k),
            "A".repeat(n)
        )
    });
}

#[test]
fn a_saved_text_state_costs_the_same_however_many_glyphs_clip() {
    assert_unit_cost_ignores_size("q (B) Tj Q after clipping glyphs", |k, n| {
        format!(
            "BT 7 Tr /H 1 Tf ({}) Tj {} ET",
            "B".repeat(k),
            "q (B) Tj Q ".repeat(n)
        )
    });
}

#[test]
fn a_saved_stroke_state_costs_the_same_under_any_dash_array() {
    assert_unit_cost_ignores_size("q 2 w Q under a dash array", |m, n| {
        format!("{} {}", dash(m), "q 2 w Q ".repeat(n))
    });
}

#[test]
fn a_stroked_glyph_run_costs_the_same_under_any_dash_array() {
    assert_unit_cost_ignores_size("stroked glyph runs under a dash array", |m, n| {
        format!("{} BT 1 Tr /H 1 Tf {} ET", dash(m), "(B) Tj ".repeat(n))
    });
}
