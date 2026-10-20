//! A generated appearance costs what its visible result needs, however large the
//! numbers in the annotation, and what the file holds, however its objects refer to
//! each other. These tests run under an allocation limit: an appearance whose cost
//! grows with those numbers makes the allocator refuse, and the test binary aborts
//! ("memory allocation of … bytes failed") instead of exhausting the machine's
//! memory. Work that allocates as it goes is measured by the bytes each thread has
//! allocated in all ([`allocated`]).

use hayro_interpret::annotation::{
    Appearances, Drawn, Look, Unsupported, View, set_most_pieces, work_done,
};
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::object::ObjectIdentifier;
use hayro_interpret::hayro_syntax::page::Page;
use hayro_interpret::hayro_syntax::work;
use hayro_interpret::{AnnotationSelection, InterpreterSettings};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

mod common;

/// The system allocator, refusing to let one thread hold more than [`LIMIT`] bytes
/// at once, and counting the bytes each thread allocates. Each test runs on a thread
/// of its own, so one test's memory never fails another's; what one thread frees of
/// what another allocated counts as freed by the one that frees it.
struct Limited;

const LIMIT: isize = 256 << 20;

thread_local! {
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
    static HELD: Cell<isize> = const { Cell::new(0) };
}

/// The bytes this thread has allocated so far, freed or not.
fn allocated() -> usize {
    ALLOCATED.with(Cell::get)
}

/// The bytes this thread holds now: allocated by it and not yet freed.
fn held() -> isize {
    HELD.with(Cell::get)
}

// SAFETY: forwards to the system allocator, refusing some requests with null as
// `GlobalAlloc::alloc` allows.
unsafe impl GlobalAlloc for Limited {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size() as isize;
        // A thread being torn down has no counters left; its allocations count for
        // none.
        let fits = HELD
            .try_with(|held| {
                let now = held.get().saturating_add(size);
                (now <= LIMIT).then(|| held.set(now)).is_some()
            })
            .unwrap_or(true);
        if !fits {
            return std::ptr::null_mut();
        }
        let _ = ALLOCATED.try_with(|a| a.set(a.get() + layout.size()));
        // SAFETY: the caller's layout, as `alloc` requires.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: allocated by `alloc` above with this layout.
        unsafe { System.dealloc(ptr, layout) };
        let _ = HELD.try_with(|held| held.set(held.get() - layout.size() as isize));
    }
}

#[global_allocator]
static ALLOCATOR: Limited = Limited;

/// A page 200 pt square.
const PAGE: kurbo::Rect = kurbo::Rect::new(0.0, 0.0, 200.0, 200.0);

/// The annotations the first page of `pdf` draws as viewed on a 200 pt page.
fn drawn<'a>(appearances: &Appearances<'a>, pdf: &'a Pdf) -> std::rc::Rc<[Drawn<'a>]> {
    appearances.page(
        &pdf.pages()[0],
        AnnotationSelection::View,
        &View::new(PAGE, 1.0),
    )
}

/// What the first annotation of the first page of `pdf` looks like as viewed on a
/// 200 pt page, [`Look::Nothing`] when it draws nothing.
fn first_look(pdf: &Pdf) -> Look<'_> {
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    drawn(&appearances, pdf)
        .iter()
        .find(|d| d.index() == 0)
        .map_or(Look::Nothing, |d| d.look().clone())
}

/// A file with one 200 pt page whose only annotation is `annot`.
fn one_annotation(annot: &str) -> Pdf {
    Pdf::new(common::one_page(
        200,
        "/Annots [4 0 R]",
        vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
    ))
    .expect("valid pdf")
}

/// The generated content of the annotation `annot` on a 200 pt page.
fn generated(annot: &str) -> String {
    match first_look(&one_annotation(annot)) {
        Look::Generated(a) => a.content.clone(),
        _ => panic!("{annot}: not generated"),
    }
}

/// The number of lines drawn, as `m` operators.
fn lines(content: &str) -> usize {
    content.lines().filter(|l| l.ends_with(" m")).count()
}

/// Comb dividers are drawn each, however close, touching or overlapping, each its
/// own stroke (`MuPDF` draws lines closer than a pixel solid when they are one path
/// or one band, grey as in bake when stroked each), those that paint where the page
/// shows the field under the clips they are drawn in, and a comb that would draw
/// more than 262 144 is refused.
#[test]
fn comb_dividers_are_drawn_each() {
    let comb = |max_len: &str, w: &str, rect: &str| {
        let pdf = one_annotation(&format!(
            "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /BC [0] >> /MaxLen {max_len} \
             /BS << /W {w} >> /Rect {rect}"
        ));
        match first_look(&pdf) {
            Look::Generated(a) => {
                let n = lines(&a.content);
                assert_eq!(a.content.matches(" l\nS\n").count(), n, "{max_len} {w}");
                Ok(n)
            }
            Look::Unsupported(why) => Err(why),
            // Nothing of it shows.
            Look::Nothing => Ok(0),
            _ => panic!("{max_len} {w}: drawn otherwise"),
        }
    };
    let field = "[20 100 180 130]";
    // Dividers that touch (a step of the width) or overlap, each.
    assert_eq!(comb("158", "1", field), Ok(157));
    assert_eq!(comb("1000", "1", field), Ok(999));
    // As many overlapping as /MaxLen less one: more than 262 144 are refused.
    assert_eq!(
        comb("2147483647", "1", field),
        Err(Unsupported::Coordinates)
    );
    // Dividers 0.001 pt wide, 0.005 pt apart, each shown: as many as /MaxLen less one.
    assert_eq!(comb("32000", "0.001", field), Ok(31_999));
    // Of a field 160 times as wide as the page, with dividers about 1 pt apart, the
    // 200 within a quarter of a point of it.
    assert_eq!(comb("32000", "0.001", "[0 100 32000 130]"), Ok(200));
    assert_eq!(
        comb("100000000", "0.000001", field),
        Err(Unsupported::Coordinates)
    );
    // A field above the page shows none of its dividers, however many.
    assert_eq!(comb("1000000", "0.000001", "[20 1000 180 1030]"), Ok(0));
    // Nor does one below it whose border is wider than it is high: the lines reach
    // past the field up and down, onto the page, but the field's box clips them.
    assert_eq!(
        comb("1000000", ".00001", "[20 -.000002 180 -.000001]"),
        Ok(0)
    );
    // Nor does one above or beside the page, turned so that the rectangle holding
    // the page, in the field's space, reaches the field.
    for rect in ["[201 20 361 50]", "[20 201 180 231]"] {
        let turned = one_annotation(&format!(
            "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /R 45 /BC [0] >> /MaxLen 1000000 \
             /BS << /W .000001 >> /Rect {rect}"
        ));
        match first_look(&turned) {
            Look::Generated(a) => assert_eq!(lines(&a.content), 0, "{rect}"),
            Look::Nothing => {}
            _ => panic!("turned comb at {rect}: drawn otherwise"),
        }
    }
}

/// A comb field's `/MaxLen` beyond a 32-bit integer makes no comb, as viewers read
/// it: the value is drawn as in a plain text field, without dividers.
#[test]
fn a_max_len_beyond_32_bits_makes_no_comb() {
    for max_len in ["4294967295", "2147483648"] {
        let content = generated(&format!(
            "/Subtype /Widget /FT /Tx /Ff 16777216 /MaxLen {max_len} /V (AB) \
             /MK << /BC [0] >> /Rect [20 100 180 130]"
        ));
        // The border, and the value's glyphs, filled.
        assert!(content.contains(" re\nS\n"), "{max_len}: {content}");
        assert!(!content.contains(" l\nS\n"), "{max_len}: {content}");
    }
}

/// Of a comb field far wider than the page, only the dividers on the page are drawn.
#[test]
fn a_comb_wider_than_the_page_draws_the_dividers_on_it() {
    // Cells 500 pt wide: the page shows the divider at 2 pt (501 in the field's frame,
    // which starts at -499), and no other.
    let content = generated(
        "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /BC [0] >> /MaxLen 400 \
         /Rect [-499 100 199503 130]",
    );
    let starts: Vec<i64> = content
        .lines()
        .filter(|l| l.ends_with(" m"))
        .map(|l| l.split(' ').next().unwrap().parse::<f64>().unwrap().round() as i64)
        .collect();
    assert_eq!(starts, [501], "{content}");
}

/// A squiggle is drawn step by step across the page only; one that would draw more
/// than 262 144 steps there is refused, and so is one whose corners a viewer adds
/// up from so many steps that it may put them a point off.
#[test]
fn a_flat_squiggle_is_drawn_at_a_bounded_cost() {
    let steps = |quad: &str| {
        let pdf = one_annotation(&format!(
            "/Subtype /Squiggly /C [0] /QuadPoints [{quad}] /Rect [0 0 200 200]"
        ));
        match first_look(&pdf) {
            Look::Generated(a) => Ok(a.content.matches(" l\n").count()),
            Look::Unsupported(why) => Err(why),
            _ => panic!("{quad}: drawn otherwise"),
        }
    };
    // Steps of 0.01 pt across a quad 4,000 pt wide: about 20 000 on the page.
    let n = steps("0 100.07 4000 100.07 0 100 4000 100").unwrap();
    assert!((19_900..20_100).contains(&n), "{n} steps");
    // Steps of 0.001 pt: about 200 000 on the page, the last a sum of as many,
    // which a viewer may put more than a point off.
    assert_eq!(
        steps("0 100.007 4000000 100.007 0 100 4000000 100"),
        Err(Unsupported::Coordinates)
    );
    // Steps of 0.0001 pt: about 2 000 000.
    assert_eq!(
        steps("0 100.0007 4000000 100.0007 0 100 4000000 100"),
        Err(Unsupported::Coordinates)
    );
    assert_eq!(
        steps("0 0.000000000001 200 0.000000000001 0 0 200 0"),
        Err(Unsupported::Coordinates)
    );
}

/// A squiggle's steps the page does not show are neither drawn nor counted against
/// the most it draws, nor looked at one by one: a quad wholly above the page, whose
/// million steps rise steeply to the right, beside a quad on the page.
#[test]
fn a_squiggle_off_the_page_costs_nothing_however_steep() {
    let shown = "20 114 180 114 20 100 180 100";
    let steep = "0 1000.007 1000 11000 0 1000 1000 1000";
    let look = |quads: &str| {
        let pdf = one_annotation(&format!(
            "/Subtype /Squiggly /C [0] /QuadPoints [{quads}] /Rect [0 0 200 200]"
        ));
        let work = work_done();
        let steps = match first_look(&pdf) {
            Look::Generated(a) => Ok(a.content.matches(" l\n").count()),
            Look::Unsupported(why) => Err(why),
            Look::Nothing => Ok(0),
            _ => panic!("{quads}: drawn otherwise"),
        };
        (steps, work_done() - work)
    };
    let (alone, one) = look(shown);
    assert_eq!(alone, Ok(80));
    let (both, two) = look(&format!("{shown} {steep}"));
    assert_eq!(both, alone);
    assert!(two < one + 200, "{one} for one quad, {two} for both");
    assert_eq!(look(steep).0, Ok(0));
}

/// A squiggle's steps whose stroke and joins paint nothing on the page are
/// neither drawn nor counted, however near their corners, nor looked at one by
/// one: quads just below the page, their steps within half the miter limit times
/// the width of it but their joins' tips further than a viewer paints beyond it,
/// beside a quad on the page, draw what that quad draws alone; so do such quads
/// slanted, their tips leaning, and with sides apart from parallel, whose steps
/// change shape along them. Where the steps come closer than a viewer paints
/// beyond the page, they paint its edge (`MuPDF` draws the thinnest strokes a
/// fifth of a pixel wide): two million such steps are refused.
#[test]
fn a_squiggle_whose_joins_stay_off_the_page_costs_nothing() {
    let shown = "20 114 180 114 20 100 180 100";
    let look = |quads: &str| {
        let work = work_done();
        let pdf = one_annotation(&format!(
            "/Subtype /Squiggly /C [0] /QuadPoints [{quads}] /Rect [0 0 200 200]"
        ));
        let corners = match first_look(&pdf) {
            Look::Generated(a) => Ok(a
                .content
                .lines()
                .filter(|l| l.ends_with(" m") || l.ends_with(" l"))
                .map(str::to_owned)
                .collect::<Vec<_>>()),
            Look::Unsupported(why) => Err(why),
            _ => panic!("{quads}: drawn otherwise"),
        };
        (corners, work_done() - work)
    };
    let (alone, one) = look(shown);
    assert_eq!(alone.as_ref().map(Vec::len), Ok(81));
    // 7 high, so 2.19 within half the miter limit times the width; the joins' tips
    // reach 0.31 from the corners on the bottom edge, 0.6 below the page.
    for below in [
        "0 -7.9 200 -7.9 0 -0.9 200 -0.9",
        // Slanted: the sides lean right by the height.
        "7 -7.9 207 -7.9 0 -0.9 200 -0.9",
        // The right side a tenth longer than the left.
        "0 -7.9 200 -8.6 0 -0.9 200 -0.9",
    ] {
        let (both, two) = look(&format!("{shown} {below}"));
        assert_eq!(both, alone, "{below}");
        assert!(
            two < one + 200,
            "{below}: {one} for one quad, {two} for both"
        );
    }
    // 0.0007 high, 0.0001 below the page: 2 000 001 steps whose hairline greys the
    // page's bottom row in bake.
    let (both, _) = look(&format!("{shown} 0 -.0008 200 -.0008 0 -.0001 200 -.0001"));
    assert_eq!(both, Err(Unsupported::Coordinates));
}

/// A squiggle finer than the 32-bit numbers it is written in, along the edge of
/// what the page shows: its corners as written lie on that grid so unevenly that
/// whether its joins' tips reach the page is told only step by step, as no bound
/// on a range of them tells it. The steps looked at one by one count against the
/// most an annotation draws, with those it draws, so such a squiggle is refused
/// within a few times that most of work, whether or not its steps paint: quads
/// 10^-5 and 10^-4 pt high, their top corners 2 and 5 half widths below what the
/// page shows grown (their joins reach up to 10), the first's steps 10^-5 · 2/7 pt,
/// under a hundredth of the grid's 1.5·10^-5 at 200, beside a quad on the page
/// that widens the clip to it. Looked at one by one, nothing of the first and
/// third paints; the second's sharpest joins do.
#[test]
fn a_squiggle_finer_than_its_numbers_along_the_page_is_refused_at_a_bounded_cost() {
    let shown = "20 114 21 114 20 100 21 100";
    let quad = |h: f64, d: f64| {
        let top = -0.25 - d * h / 32.0;
        let (low, high) = (top - h / 7.0, top - h / 7.0 + h);
        format!("0 {high} 200 {high} 0 {low} 200 {low} {shown}")
    };
    let look = |quads: &str| {
        let pdf = one_annotation(&format!(
            "/Subtype /Squiggly /C [0] /QuadPoints [{quads}] /Rect [0 0 200 200]"
        ));
        let work = work_done();
        let look = match first_look(&pdf) {
            Look::Unsupported(why) => Err(why),
            Look::Generated(_) => Ok(()),
            _ => panic!("{quads}: drawn otherwise"),
        };
        (look, work_done() - work)
    };
    let fine = [(1e-5, 2.0), (1e-4, 2.0), (1e-4, 5.0)];
    set_most_pieces(4096);
    for (h, d) in fine {
        let (got, work) = look(&quad(h, d));
        assert_eq!(got, Err(Unsupported::Coordinates), "{h} {d}");
        assert!(work < 2 * 4096 + 1000, "{h} {d}: {work}");
    }
    // Tens of thousands of times coarser, the steps are told apart by range, past
    // what a viewer adding them up from the quad's start can move them: drawn at
    // a cost that does not grow with the limit.
    let (got, work) = look(&quad(3.0, 2.0));
    assert_eq!(got, Ok(()));
    assert!(work < 1000, "{work}");
    // At the default most, as within it: 0.8 s in a release build, 6.4 s in a
    // debug one, when this was written.
    set_most_pieces(262_144);
    let (got, work) = look(&quad(1e-4, 2.0));
    assert_eq!(got, Err(Unsupported::Coordinates));
    assert!(work < 2 * 262_144 + 1000, "{work}");
}

/// 10^5: a place this far from the page is still within a point of where any
/// viewer puts it.
fn huge() -> String {
    "100000".to_owned()
}

/// A radio button 200,000 pt across, as large as its place stays within a point of
/// where any viewer puts it, is four curves for each circle, as viewers draw it:
/// the work does not grow with its size.
#[test]
fn a_huge_radio_button_costs_what_a_small_one_does() {
    let h = huge();
    let radio = generated(&format!(
        "/Subtype /Widget /FT /Btn /Ff 32768 /AS /On /MK << /BG [0] /BC [1 0 0] >> \
         /Rect [-{h} -{h} {h} {h}]"
    ));
    // The disc, the ring and the dot.
    assert_eq!(radio.matches(" c\n").count(), 12, "{radio}");
}

/// So is a circle line ending of a line 30,000 pt wide.
#[test]
fn a_huge_circle_ending_costs_what_a_small_one_does() {
    let h = "30000";
    let ending = generated(&format!(
        "/Subtype /Line /C [0] /BS << /W {h} >> /LE [/Circle /None] /L [50 50 100 50] \
         /Rect [0 0 200 200]"
    ));
    assert_eq!(ending.matches(" c\n").count(), 4, "{ending}");
}

/// Widgets below one deep chain of fields read each field once: the work grows with
/// the fields and widgets, not with their product.
#[test]
fn widgets_below_one_chain_of_fields_read_each_field_once() {
    const N: usize = 1000;
    let mut objects = Vec::new();
    let widgets: Vec<String> = (0..N).map(|i| format!("{} 0 R", 4 + i)).collect();
    for _ in 0..N {
        objects.push(
            format!(
                "<< /Type /Annot /Subtype /Widget /Parent {} 0 R /MK << /BG [1 0 0] >> \
                 /Rect [10 10 110 30] >>",
                4 + N
            )
            .into_bytes(),
        );
    }
    for k in 0..N {
        let n = 4 + N + k;
        objects.push(if k == N - 1 {
            b"<< /FT /Tx >>".to_vec()
        } else {
            format!("<< /Parent {} 0 R >>", n + 1).into_bytes()
        });
    }
    let pdf = Pdf::new(common::one_page(
        200,
        &format!("/Annots [{}]", widgets.join(" ")),
        objects,
    ))
    .expect("valid pdf");
    let before = allocated();
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let drawn = drawn(&appearances, &pdf);
    assert_eq!(drawn.len(), N);
    for d in drawn.iter() {
        assert!(matches!(d.look(), Look::Generated(_)));
    }
    let bytes = allocated() - before;
    // About 4 KiB a widget; reading the whole chain for each takes gigabytes.
    assert!(bytes < 32 << 20, "{bytes} bytes");
}

/// Pages whose widgets are below one deep chain of fields, interpreted with one cache
/// for the document as a renderer interprets them, read each field once: the work
/// grows with the fields and pages, not with their product.
#[test]
fn pages_below_one_chain_of_fields_read_each_field_once() {
    use hayro_interpret::{Context, DummyDevice, InterpreterCache, interpret_page};
    const N: usize = 1000;
    // The catalog, the page tree, N pages, a widget on each, and N fields above them.
    let (page, widget, field) = (3, 3 + N, 3 + 2 * N);
    let kids: Vec<String> = (0..N).map(|i| format!("{} 0 R", page + i)).collect();
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count {N} >>", kids.join(" ")).into_bytes(),
    ];
    for i in 0..N {
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [{} 0 R] >>",
                widget + i
            )
            .into_bytes(),
        );
    }
    for _ in 0..N {
        objects.push(
            format!(
                "<< /Type /Annot /Subtype /Widget /Parent {field} 0 R \
                 /MK << /BG [1 0 0] >> /Rect [10 10 110 30] >>"
            )
            .into_bytes(),
        );
    }
    for k in 0..N {
        objects.push(if k == N - 1 {
            b"<< /FT /Tx >>".to_vec()
        } else {
            format!("<< /Parent {} 0 R >>", field + k + 1).into_bytes()
        });
    }
    let pdf = Pdf::new(common::file(&objects, "")).expect("valid pdf");
    let pages = pdf.pages();
    let cache = InterpreterCache::new();
    let before = allocated();
    for page in pages.iter() {
        let mut ctx = Context::new(
            kurbo::Affine::IDENTITY,
            PAGE,
            &cache,
            page.xref(),
            InterpreterSettings::default(),
        );
        interpret_page(page, &View::new(PAGE, 1.0), &mut ctx, &mut DummyDevice);
    }
    let bytes = allocated() - before;
    // About 15 KiB a page; reading the whole chain for each page takes 1.7 GB.
    assert!(bytes < 64 << 20, "{bytes} bytes");
}

/// The bytes allocated to look at each of `n` widgets below one field whose entries
/// are `field`, among them a value of `len` characters the widgets share, `/Rect`
/// `rect` each: with the value set once for the document and only what the field
/// shows of it drawn, about what one widget takes plus some 30 KiB a widget.
fn shared_value_cost(n: usize, len: usize, field: &str, rect: &str) -> usize {
    let value: String = "ABCD efgh ".chars().cycle().take(len).collect();
    let widgets: Vec<String> = (0..n).map(|i| format!("{} 0 R", 5 + i)).collect();
    let mut objects =
        vec![format!("<< /FT /Tx /V ({value}) /DA (/Helv 12 Tf 0 g) {field} >>").into_bytes()];
    for _ in 0..n {
        objects.push(
            format!("<< /Type /Annot /Subtype /Widget /Parent 4 0 R /Rect {rect} >>").into_bytes(),
        );
    }
    let pdf = Pdf::new(common::one_page(
        200,
        &format!("/Annots [{}]", widgets.join(" ")),
        objects,
    ))
    .expect("valid pdf");
    let before = allocated();
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let drawn = drawn(&appearances, &pdf);
    assert_eq!(drawn.len(), n);
    for d in drawn.iter() {
        let Look::Generated(a) = d.look() else {
            panic!("{field}: not generated");
        };
        assert!(
            a.content.len() < 64 << 10,
            "{field}: {} bytes",
            a.content.len()
        );
    }
    allocated() - before
}

/// Widgets sharing one long value set it once and draw what their field shows of
/// it: single line, multiline, comb, comb without `/MaxLen`, and on a field far wider
/// than the page.
#[test]
fn widgets_sharing_a_long_value_set_it_once() {
    const N: usize = 200;
    const LEN: usize = 20_000;
    for (field, rect) in [
        ("", "[10 10 110 30]"),
        ("/Ff 4096", "[10 10 110 60]"),
        ("/Ff 16777216 /MaxLen 20", "[10 10 110 30]"),
        ("/Ff 16777216", "[10 10 110 30]"),
        ("", "[-10000 10 1000000 30]"),
    ] {
        let one = shared_value_cost(1, LEN, field, rect);
        let all = shared_value_cost(N, LEN, field, rect);
        // Setting the value for each widget allocates at least 0.5 MB a widget.
        assert!(
            all < one + N * (64 << 10),
            "{field} {rect}: {one} bytes for one, {all} for {N}"
        );
    }
}

/// The bytes allocated to look at each of `n` annotations `annot`, each referring to
/// object 4, `shared`, as `4 0 R`.
fn shared_object_cost(n: usize, shared: &str, annot: &str) -> usize {
    let refs: Vec<String> = (0..n).map(|i| format!("{} 0 R", 5 + i)).collect();
    let mut objects = vec![shared.as_bytes().to_vec()];
    for _ in 0..n {
        objects.push(format!("<< /Type /Annot {annot} >>").into_bytes());
    }
    let pdf = Pdf::new(common::one_page(
        200,
        &format!("/Annots [{}]", refs.join(" ")),
        objects,
    ))
    .expect("valid pdf");
    let before = allocated();
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let drawn = drawn(&appearances, &pdf);
    assert_eq!(drawn.len(), n);
    for d in drawn.iter() {
        let Look::Generated(a) = d.look() else {
            panic!("{annot}: not generated");
        };
        assert!(
            a.content.len() < 64 << 10,
            "{annot}: {} bytes",
            a.content.len()
        );
    }
    allocated() - before
}

/// Annotations whose text is one object they all refer to lay it out once and draw
/// what their boxes show of it: `FreeText` contents and rich text, a rich text
/// field's value, and a list's options.
#[test]
fn annotations_sharing_a_long_text_set_it_once() {
    const N: usize = 200;
    const LEN: usize = 20_000;
    let text: String = "ABCD efgh ".chars().cycle().take(LEN).collect();
    let options: String = (0..LEN / 10).map(|i| format!("(item{i}) ")).collect();
    let da = "/Rect [10 10 110 40] /DA (/Helv 12 Tf 0 g)";
    for (shared, annot) in [
        (
            format!("({text})"),
            format!("/Subtype /FreeText {da} /Contents 4 0 R"),
        ),
        (
            format!("(<p>{text}</p>)"),
            format!("/Subtype /FreeText {da} /Contents (x) /RC 4 0 R"),
        ),
        (
            format!("(<p>{text}</p>)"),
            format!("/Subtype /Widget /FT /Tx /Ff 33554432 {da} /V (x) /RV 4 0 R"),
        ),
        (
            format!("[{options}]"),
            format!("/Subtype /Widget /FT /Ch {da} /Opt 4 0 R"),
        ),
    ] {
        let one = shared_object_cost(1, &shared, &annot);
        let all = shared_object_cost(N, &shared, &annot);
        // Laying the text out for each annotation allocates at least 0.6 MB one;
        // drawing what a box shows, as outlines, up to some 64 KiB.
        assert!(
            all < one + N * (128 << 10),
            "{annot}: {one} bytes for one, {all} for {N}"
        );
    }
}

/// A Squiggly quad whose top line stands still while its bottom edge runs a million
/// points: its hundred million steps all fan out of one point on the page, more than
/// the page can tell apart. Such a quad marks no text and is unsupported, not drawn
/// at the cost of every step.
#[test]
fn a_squiggly_fanning_out_of_a_point_is_unsupported() {
    let pdf = Pdf::new(common::one_page(
        200,
        "/Annots [4 0 R]",
        vec![
            b"<< /Type /Annot /Subtype /Squiggly /C [0] /Rect [0 0 200 200] \
               /QuadPoints [100 100.0707 -5999900 100.0707 100 100 1000100 100] >>"
                .to_vec(),
        ],
    ))
    .expect("valid pdf");
    assert!(matches!(
        first_look(&pdf),
        Look::Unsupported(Unsupported::Coordinates)
    ));
}

/// What `annot` looks like on a 200 pt page: its generated content, or why it has
/// none. Fails the test unless it is done within `secs` seconds.
fn look_within(secs: u64, annot: String) -> Result<String, Unsupported> {
    let (sent, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pdf = one_annotation(&annot);
        let look = match first_look(&pdf) {
            Look::Generated(a) => Ok(a.content.clone()),
            Look::Unsupported(why) => Err(why),
            _ => panic!("neither generated nor unsupported"),
        };
        let _ = sent.send(look);
    });
    done.recv_timeout(std::time::Duration::from_secs(secs))
        .expect("done in time")
}

/// A `FreeText` with rich text `rc`, a UTF-8 text string.
fn rich(rc: &str) -> String {
    format!("/Subtype /FreeText /Rect [20 20 180 180] /BS << /W 0 >> /RC (\u{FEFF}{rc})")
}

/// Reading rich text costs what its length does, however many ampersands start no
/// character reference: here the text is read, and then its first character found
/// in no font.
#[test]
fn rich_text_with_many_ampersands_is_read_at_a_bounded_cost() {
    let rc = format!("<p>\u{6F22}{}</p>", "&".repeat(1_600_000));
    assert_eq!(
        look_within(10, rich(&rc)),
        Err(Unsupported::Character('\u{6F22}'))
    );
}

/// Elements nested beyond what written rich text does make it unsupported before
/// the parser's work, which grows with the depth for each element, outgrows the
/// text.
#[test]
fn deeply_nested_rich_text_is_unsupported_at_a_bounded_cost() {
    let rc = format!("{}{}", "<span>".repeat(100_000), "</p>".repeat(100_000));
    assert_eq!(look_within(20, rich(&rc)), Err(Unsupported::RichText));
}

/// Formatting elements left open are copied into each paragraph after them, as
/// viewers read HTML; copies beyond what the text could hold make it unsupported
/// before they fill the memory.
#[test]
fn formatting_left_open_across_paragraphs_is_unsupported_at_a_bounded_cost() {
    let open: String = (0..200).map(|i| format!("<b title={i}>")).collect();
    let rc = format!("<p>{open}{}", "</p><p>x".repeat(50_000));
    assert_eq!(look_within(20, rich(&rc)), Err(Unsupported::RichText));
}

/// One tag with many attributes is read in time that grows with their number, not
/// its square: 128 000 of them, 1.2 MB of text, took 8 s in a release build when
/// each attribute was compared with every earlier one of its tag, and take 0.3 s
/// in a debug build now.
#[test]
fn rich_text_with_a_tag_of_many_attributes_is_read_at_a_bounded_cost() {
    let attributes: String = (0..128_000).map(|i| format!(" a{i}=x")).collect();
    let rc = format!("<p{attributes}>A</p>");
    assert!(look_within(3, rich(&rc)).is_ok());
}

/// Attributes `abc` + `z` + `abc`, for all 46 656 strings `abc` of three lowercase
/// letters or digits, whose names all have one atom hash: `string_cache` 0.9 hashes
/// a name of up to seven bytes to the XOR of the halves of its 64-bit atom.
fn attributes_whose_names_hash_alike() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut attributes = String::new();
    for a in CHARS {
        for b in CHARS {
            for c in CHARS {
                let abc = String::from_utf8(vec![*a, *b, *c]).unwrap();
                attributes.push_str(&format!(" {abc}z{abc}=x"));
            }
        }
    }
    attributes
}

/// One tag with attributes whose names hash alike is read in time that grows with
/// their number: html5ever's set of a large tag's names, keyed by the names' atom
/// hash, took 11 s for these in a debug build, and keyed by their text, 0.1 s.
#[test]
fn rich_text_with_a_tag_of_names_whose_atoms_hash_alike_is_read_at_a_bounded_cost() {
    let rc = format!("<p{}>A</p>", attributes_whose_names_hash_alike());
    assert!(look_within(3, rich(&rc)).is_ok());
}

/// A repeated tag merges in attributes whose names hash alike in time that grows
/// with their number: a set of names keyed by their atoms' hash took 16 s for
/// these in a debug build, and keyed by their text, 0.1 s.
#[test]
fn repeated_tags_with_names_whose_atoms_hash_alike_are_merged_at_a_bounded_cost() {
    let rc = format!(
        "<body{}><body a=b><p>A</p>",
        attributes_whose_names_hash_alike()
    );
    assert!(look_within(3, rich(&rc)).is_ok());
}

/// One tag with attributes whose names, longer than seven bytes, share a bucket
/// of `string_cache` 0.9's set of atoms is read in time that grows with their number.
/// The set put such a name in one of 4,096 lists by a hash with a fixed key, so
/// each name stepped past all those before it: these 65 536 names took 10 s in a
/// debug build, and 0.3 s with the set placing atoms by a per-process random hash.
/// The file holds names `n` + 11 hex digits of a counter, found offline to share
/// one list under the key of `web_atoms` 0.2.6's local names, as LEB128 differences
/// between the counters.
#[test]
fn rich_text_with_a_tag_of_names_in_one_atom_bucket_is_read_at_a_bounded_cost() {
    let mut attributes = String::new();
    let (mut counter, mut delta, mut shift) = (0_u64, 0_u64, 0);
    for byte in include_bytes!("one_bucket_attribute_names.bin") {
        delta |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            counter += delta;
            attributes.push_str(&format!(" n{counter:011x}=x"));
            (delta, shift) = (0, 0);
        }
    }
    let rc = format!("<p{attributes}>A</p>");
    assert!(look_within(3, rich(&rc)).is_ok());
}

/// Text beside or below a `FreeText`'s box is clipped away, so it is not drawn:
/// the content stays the size of what the box shows, however long the text.
#[test]
fn text_draws_only_what_its_box_shows() {
    let box_ = "/Rect [20 20 60 180] /BS << /W 0 >> /DA (/Helv 12 Tf 0 g)";
    let options = "(A) ".repeat(100_000);
    for (contents, kind) in [
        ("A".repeat(200_000), "/Subtype /FreeText /Contents"),
        ("A ".repeat(100_000), "/Subtype /FreeText /Contents"),
        ("A".repeat(200_000), "/Subtype /Widget /FT /Tx /V"),
        ("A".repeat(200_000), "/Subtype /Widget /FT /Tx /Ff 4096 /V"),
        (String::new(), "/Subtype /Widget /FT /Ch /V"),
    ] {
        let annot = if contents.is_empty() {
            format!("{kind} () /Opt [{options}] {box_}")
        } else {
            format!("{kind} ({contents}) {box_}")
        };
        let content = generated(&annot);
        assert!(content.len() < 100_000, "{kind}: {} bytes", content.len());
    }
}

/// A stream where a number, flags, a colour, a name or a dictionary is read is not
/// decoded: an annotation whose entries are a stream of 32 MiB compressed to little
/// costs what one without them does.
#[test]
fn a_stream_read_for_no_text_is_not_decoded() {
    use std::io::Write;
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    zlib.write_all(&vec![0; 32 << 20]).unwrap();
    let data = zlib.finish().unwrap();
    let square = "/Subtype /Square /Rect [20 20 100 100]";
    let button = "/Subtype /Widget /FT /Btn /Rect [20 20 180 60] /MK << /BG [0.5] /CA (A) >>";
    for annot in [
        format!("{square} /IC [1 0 0] /F 5 0 R"),
        format!("{square} /IC 5 0 R /C [0]"),
        format!("{square} /IC [1 0 0] /C [0] /BS 5 0 R /CA 5 0 R"),
        format!("{square} /IC [1 0 0] /C [0] /Border [0 0 5 0 R]"),
        "/Subtype /Line /Rect [20 20 100 100] /C [0] /L [20 20 100 100] /LE 5 0 R".to_owned(),
        format!("{button} /Ff 5 0 R"),
        format!("{button} /Ff 65536 /DA 5 0 R"),
    ] {
        let pdf = Pdf::new(common::one_page(
            200,
            "/Annots [4 0 R]",
            vec![
                format!("<< /Type /Annot {annot} >>").into_bytes(),
                common::stream("/Filter /FlateDecode", &data),
            ],
        ))
        .expect("valid pdf");
        let appearances =
            Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
        let page = &pdf.pages()[0];
        let before = allocated();
        let flattened = appearances.page(page, AnnotationSelection::Flatten, &View::new(PAGE, 1.0));
        let viewed = appearances.page(page, AnnotationSelection::View, &View::new(PAGE, 1.0));
        for drawn in [flattened, viewed] {
            assert!(
                matches!(drawn.first().map(Drawn::look), Some(Look::Generated(_))),
                "{annot}"
            );
        }
        let bytes = allocated() - before;
        assert!(bytes < 1 << 20, "{bytes} bytes: {annot}");
    }
}

/// What looking at annotations costs: the bytes allocated, and the units of work
/// done ([`hayro_interpret::annotation::work_done`]).
#[derive(Debug, Clone, Copy)]
struct Cost {
    bytes: usize,
    work: u64,
}

/// The cost of looking at each of `n` annotations `annot(i)` on a 200 pt page of a
/// file whose catalog has `catalog` besides its pages, and whose objects from 4 are
/// `shared`, the annotations after them. None may be unsupported.
fn cost_of(n: usize, catalog: &str, shared: &[String], annot: &dyn Fn(usize) -> String) -> Cost {
    cost_refused(n, catalog, shared, annot, false)
}

/// [`cost_of`], where each annotation may also be refused for its coordinates
/// when `refused`: a long text centred by its advance, which a viewer measures by
/// adding it up, lies where no viewer can be told to put it, but is set as any.
fn cost_refused(
    n: usize,
    catalog: &str,
    shared: &[String],
    annot: &dyn Fn(usize) -> String,
    refused: bool,
) -> Cost {
    let first = 4 + shared.len();
    let refs: Vec<String> = (0..n).map(|i| format!("{} 0 R", first + i)).collect();
    let mut objects = vec![
        format!("<< /Type /Catalog /Pages 2 0 R {catalog} >>").into_bytes(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [{}] >>",
            refs.join(" ")
        )
        .into_bytes(),
    ];
    objects.extend(shared.iter().map(|s| s.clone().into_bytes()));
    objects.extend((0..n).map(|i| format!("<< /Type /Annot {} >>", annot(i)).into_bytes()));
    let pdf = Pdf::new(common::file(&objects, "")).expect("valid pdf");
    let (bytes, work) = (allocated(), work_done());
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    for d in drawn(&appearances, &pdf).iter() {
        match d.look() {
            Look::Generated(g) => assert!(g.content.len() < 64 << 10, "{}", annot(0)),
            Look::Unsupported(Unsupported::Coordinates) if refused => {}
            Look::Unsupported(why) => panic!("{why:?}: {}", annot(0)),
            _ => {}
        }
    }
    Cost {
        bytes: allocated() - bytes,
        work: work_done() - work,
    }
}

/// Asserts that twice as many annotations `annot(i)` sharing `shared` cost no more
/// than drawing what the added ones show: about 64 KiB and some thousands of units
/// of work each, however long what they share. Annotations that each set, lay out or
/// read the whole of a shared entry of 20,000 characters or numbers take ten times
/// that.
fn shared_once(catalog: &str, shared: &[String], annot: impl Fn(usize) -> String) {
    shared_once_or_refused(catalog, shared, annot, false);
}

/// [`shared_once`], where each annotation may be refused for its coordinates when
/// `refused` ([`cost_refused`]).
fn shared_once_or_refused(
    catalog: &str,
    shared: &[String],
    annot: impl Fn(usize) -> String,
    refused: bool,
) {
    const N: usize = 40;
    let (one, two) = (
        cost_refused(N, catalog, shared, &annot, refused),
        cost_refused(2 * N, catalog, shared, &annot, refused),
    );
    let (bytes, work) = (two.bytes - one.bytes, two.work - one.work);
    assert!(
        bytes < N * (64 << 10) && work < (N * 4_000) as u64,
        "{}: {one:?} for {N}, {two:?} for {}",
        annot(0),
        2 * N
    );
}

/// A long text of words, 20,000 characters as written, with escapes, so that
/// reading it allocates.
fn long_text() -> String {
    "ABCD efg\\) ".repeat(1_818)
}

/// Annotations that share a text set it once for the document, and lay out and
/// draw only what each shows, whatever width, box, size, colour or alignment each
/// draws it in: `FreeText` contents and rich text, a field's value and rich value,
/// a push button's caption and a line's caption.
#[test]
fn a_shared_text_is_set_once_whatever_each_annotation_draws_it_in() {
    let text = vec![format!("({})", long_text())];
    let rich = vec![format!("(<p>{}</p>)", long_text())];
    let thousandths = |i: usize| format!("{}.{:03}", 100 + i / 1000, i % 1000);
    let free_text = "/Subtype /FreeText /BS << /W 0 >>";
    shared_once("", &text, |i| {
        format!(
            "{free_text} /Contents 4 0 R /Rect [10 10 {} 40] /DA (/Helv 12 Tf 0 g)",
            thousandths(i)
        )
    });
    shared_once("", &text, |i| {
        format!(
            "{free_text} /Contents 4 0 R /Rect [10 10 110 40] /DA (/Helv {} Tf 0 g)",
            thousandths(i)
        )
    });
    shared_once("", &text, |i| {
        format!(
            "{free_text} /Contents 4 0 R /Rect [10 10 110 40] /Q {} /DA (/Helv 12 Tf 0.{i:03} g)",
            i % 3
        )
    });
    shared_once("", &rich, |i| {
        format!(
            "{free_text} /RC 4 0 R /Rect [10 10 {} 40] /DA (/Helv 12 Tf 0 g)",
            thousandths(i)
        )
    });
    shared_once("", &rich, |i| {
        format!(
            "{free_text} /RC 4 0 R /Rect [10 10 110 40] /DA (/Helv {} Tf 0 g)",
            thousandths(i)
        )
    });
    let field = "/Subtype /Widget /FT /Tx";
    for flags in ["", "/Ff 4096", "/Ff 16777216 /MaxLen 20", "/Ff 16777216"] {
        shared_once("", &text, |i| {
            format!(
                "{field} {flags} /V 4 0 R /Rect [10 10 {} 40] /DA (/Helv {} Tf 0 g)",
                thousandths(i),
                thousandths(i).replace("100.", "10.")
            )
        });
    }
    shared_once("", &rich, |i| {
        format!(
            "{field} /Ff 33554432 /V (x) /RV 4 0 R /Rect [10 10 {} 40] /DA (/Helv {} Tf 0 g)",
            thousandths(i),
            thousandths(i).replace("100.", "10.")
        )
    });
    // A caption is centred by its advance: this one, refused.
    let caption = |i| {
        format!(
            "/Subtype /Widget /FT /Btn /Ff 65536 /MK << /CA 4 0 R >> /Rect [10 10 {} 40]",
            thousandths(i)
        )
    };
    shared_once_or_refused("", &text, caption, true);
    // So is a line's caption.
    let line_caption = |i| {
        format!(
            "/Subtype /Line /C [0] /Cap true /Contents 4 0 R /L [20 100 {} 100] /Rect [0 0 200 200]",
            thousandths(i)
        )
    };
    shared_once_or_refused("", &text, line_caption, true);
}

/// Annotations that share a text, each with a default style written in it, set the
/// text once when their styles are equal, at either length of the text: `FreeText`
/// contents and rich text, a field's value and rich value.
#[test]
fn a_shared_text_is_set_once_whatever_entry_holds_an_equal_style() {
    for words in [long_text(), long_text().repeat(2)] {
        let text = vec![format!("({words})")];
        let rich = vec![format!("(<p>{words}</p>)")];
        for ds in ["/DS ()", "/DS (font-size:12pt)"] {
            let free_text = format!("/Subtype /FreeText /BS << /W 0 >> {ds} /DA (/Helv 12 Tf)");
            let field = format!("/Subtype /Widget /FT /Tx {ds} /DA (/Helv 12 Tf)");
            for (shared, annot) in [
                (
                    &text,
                    format!("{free_text} /Contents 4 0 R /Rect [10 10 110 40]"),
                ),
                (&rich, format!("{free_text} /RC 4 0 R /Rect [10 10 110 40]")),
                (&text, format!("{field} /V 4 0 R /Rect [10 10 110 40]")),
                (
                    &rich,
                    format!("{field} /V (x) /RV 4 0 R /Rect [10 10 110 40]"),
                ),
                (
                    &rich,
                    format!("{field} /Ff 33554432 /V (x) /RV 4 0 R /Rect [10 10 110 40]"),
                ),
            ] {
                shared_once("", shared, |_| annot.clone());
            }
        }
    }
}

/// A list's options are read once for the document, and of them only those its box
/// shows are set, each label once however many options or lists refer to it: the
/// work grows with the options, not with their labels' length times their number.
#[test]
fn a_list_sets_only_the_options_it_shows() {
    let label = format!("({})", long_text());
    for opt in [
        |n: usize| vec!["4 0 R"; n].join(" "),
        |n: usize| vec!["[(e) 4 0 R]"; n].join(" "),
    ] {
        for rich in ["", "/DS (font-size:12pt)"] {
            let cost = |n: usize| {
                cost_of(1, "", std::slice::from_ref(&label), &|_| {
                    format!(
                        "/Subtype /Widget /FT /Ch /Opt [{}] {rich} /Rect [10 10 110 60] \
                         /DA (/Helv 12 Tf 0 g)",
                        opt(n)
                    )
                })
            };
            let (one, two) = (cost(1000), cost(2000));
            // Some 200 bytes an option, kept for the document, and the node its
            // entry is parsed into once, some 450 bytes for a pair; work for the
            // dozen bytes it is written in; setting each option would take some
            // 20,000 characters' worth.
            assert!(
                two.bytes - one.bytes < 1000 * 1024 && two.work - one.work < 1000 * 64,
                "{rich}: {one:?} {two:?}"
            );
        }
    }
    // Lists sharing the options, scrolled by /TI to each option in turn.
    let options = vec![format!("[{}]", vec!["5 0 R"; 200].join(" ")), label];
    for rich in ["", "/DS (font-size:12pt)"] {
        shared_once("", &options, |i| {
            format!(
                "/Subtype /Widget /FT /Ch /Opt 4 0 R /TI {i} {rich} /Rect [10 10 110 60] \
                 /DA (/Helv 12 Tf 0 g)"
            )
        });
    }
}

/// Annotations that share an object read it once for the document: a default
/// appearance of their own, a field's, or the form's; a border style, appearance
/// characteristics or appearance dictionary of 20,000 entries; a `/Rect`, colour,
/// `/RD` or `/CL` that refers to an array of 20,000 numbers.
#[test]
fn a_shared_entry_is_read_once_or_as_far_as_needed() {
    let da = format!("({}/Helv 12 Tf 0 g)", "0 g ".repeat(5_000));
    shared_once("", std::slice::from_ref(&da), |i| {
        format!("/Subtype /FreeText /Rect [10 10 110 40] /Contents (A{i}) /DA 4 0 R")
    });
    shared_once("", &[format!("<< /FT /Tx /DA {da} >>")], |i| {
        format!("/Subtype /Widget /Parent 4 0 R /V (A{i}) /Rect [10 10 110 40]")
    });
    shared_once(&format!("/AcroForm << /DA {da} >>"), &[], |i| {
        format!("/Subtype /Widget /FT /Tx /V (A{i}) /Rect [10 10 110 40]")
    });
    // Dictionaries of 20,000 entries: a border style, appearance characteristics,
    // appearance states.
    let many = |entries: &str| {
        let mut d = format!("<< {entries}");
        for i in 0..20_000 {
            d.push_str(&format!(" /K{i} 0"));
        }
        d + " >>"
    };
    shared_once("", &[many("/W 2")], |_| {
        "/Subtype /Square /C [0] /Rect [10 10 110 40] /BS 4 0 R".to_owned()
    });
    shared_once("", &[many("/BG [1 0 0]")], |_| {
        "/Subtype /Widget /FT /Btn /Ff 65536 /Rect [10 10 110 40] /MK 4 0 R".to_owned()
    });
    shared_once("", &[format!("<< /N {} >>", many(""))], |_| {
        "/Subtype /Square /C [0] /Rect [10 10 110 40] /AP 4 0 R /AS /Off".to_owned()
    });
    let numbers = format!("[10 10 110 40 {}]", "0 ".repeat(20_000));
    shared_once("", std::slice::from_ref(&numbers), |_| {
        "/Subtype /Square /C [0] /Rect 4 0 R".to_owned()
    });
    shared_once("", std::slice::from_ref(&numbers), |_| {
        "/Subtype /Square /C 4 0 R /IC 4 0 R /Rect [10 10 110 40]".to_owned()
    });
    shared_once("", std::slice::from_ref(&numbers), |i| {
        format!("/Subtype /FreeText /Rect [10 10 110 40] /RD 4 0 R /Contents (A{i})")
    });
    shared_once("", &[numbers], |i| {
        format!(
            "/Subtype /FreeText /IT /FreeTextCallout /CL 4 0 R /Rect [10 10 110 40] /Contents (A{i})"
        )
    });
}

/// Text in a negative size (upside down, its lines going up) is laid out as far as
/// its box shows, as text in a positive one is; rich text with sizes both ways,
/// which no producer writes and whose lines no search bounds, is unsupported.
#[test]
fn text_in_a_negative_size_is_laid_out_as_far_as_it_shows() {
    let text = vec![format!("({})", long_text().replace(' ', "\\n"))];
    shared_once("", &text, |_| {
        "/Subtype /FreeText /Contents 4 0 R /Rect [10 10 110 40] /DA (/Helv -12 Tf 0 g)".to_owned()
    });
    shared_once("", &text, |_| {
        "/Subtype /Widget /FT /Tx /Ff 4096 /V 4 0 R /Rect [10 10 110 40] /DA (/Helv 12 Tf 0 g)"
            .to_owned()
    });
    shared_once("", &text, |_| {
        "/Subtype /Widget /FT /Tx /Ff 4096 /V 4 0 R /Rect [10 10 110 40] /DA (/Helv -12 Tf 0 g)"
            .to_owned()
    });
    let mixed = format!(
        "/Subtype /FreeText /Rect [10 10 110 40] /DA (/Helv -12 Tf) \
         /RC (<p>{}<span style=\"font-size:12pt\">A</span></p>)",
        long_text()
    );
    assert!(matches!(
        first_look(&one_annotation(&mixed)),
        Look::Unsupported(Unsupported::RichText)
    ));
}

/// A glyph is written once for an appearance, as a form, and each place it is drawn
/// costs a few bytes: a comb of 10,000 cells, all shown, and a `FreeText` full of
/// text, each some 36 bytes a glyph drawn (each glyph after a run's first in a `q`
/// of its own, so that a viewer places it from the first, not from each before
/// it), as `bake` writes some 11 (before either is compressed).
#[test]
fn a_drawn_glyph_costs_a_few_bytes() {
    let value: String = "ABCDEFGHIJ0123456789"
        .chars()
        .cycle()
        .take(10_000)
        .collect();
    for annot in [
        format!(
            "/Subtype /Widget /FT /Tx /Ff 16777216 /MaxLen 10000 /V ({value}) \
             /DA (/Helv 0 Tf 0 g) /Rect [0 0 200 30]"
        ),
        format!(
            "/Subtype /FreeText /Rect [0 0 200 200] /DA (/Helv 4 Tf 0 g) /Contents ({})",
            long_text()
        ),
    ] {
        let pdf = one_annotation(&annot);
        let Look::Generated(g) = first_look(&pdf) else {
            panic!("{annot}: not generated")
        };
        let drawn = g.content.matches(" Do\n").count();
        assert!(drawn > 1_000, "{drawn}");
        assert!(g.glyphs.len() <= 20, "{}", g.glyphs.len());
        let per_glyph = g.content.len() / drawn;
        assert!(per_glyph <= 40, "{per_glyph} bytes a glyph");
    }
}

/// A text is set once for the entries it is read from, and only those: annotations
/// sharing rich text, each with a plain fallback of its own that the rich text
/// hides, set the rich text once.
#[test]
fn a_text_is_keyed_by_the_entries_it_is_read_from() {
    let rich = vec![format!("(<p>{}</p>)", long_text())];
    shared_once("", &rich, |i| {
        format!(
            "/Subtype /FreeText /BS << /W 0 >> /RC 4 0 R /Contents (A{i}) \
             /Rect [10 10 110 40] /DA (/Helv 12 Tf 0 g)"
        )
    });
    shared_once("", &rich, |i| {
        format!(
            "/Subtype /Widget /FT /Tx /Ff 33554432 /V (A{i}) /RV 4 0 R \
             /Rect [10 10 110 40] /DA (/Helv 12 Tf 0 g)"
        )
    });
}

/// An option held in a pair of its own, which many entries of `/Opt` refer to, is
/// one text: lists scrolled to different entries, or holding the pair in `/Opt`
/// arrays of their own, set it once.
#[test]
fn an_option_in_a_shared_pair_is_set_once() {
    let pair = format!("[(e) ({})]", long_text());
    let opt = format!("[{}]", vec!["5 0 R"; 200].join(" "));
    for rich in ["", "/DS (font-size:12pt)"] {
        shared_once("", &[opt.clone(), pair.clone()], |i| {
            format!(
                "/Subtype /Widget /FT /Ch /Opt 4 0 R /TI {i} {rich} /Rect [10 10 110 60] \
                 /DA (/Helv 12 Tf 0 g)"
            )
        });
        shared_once("", std::slice::from_ref(&pair), |_| {
            format!(
                "/Subtype /Widget /FT /Ch /Opt [4 0 R 4 0 R] {rich} /Rect [10 10 110 60] \
                 /DA (/Helv 12 Tf 0 g)"
            )
        });
    }
}

/// Rich text of 20,000 spans, one word that overflows its line: an annotation that
/// shows the start of it sizes its line and finds the spans it draws by searches,
/// not by a pass over the spans its box does not show.
#[test]
fn spans_a_box_does_not_show_are_not_walked() {
    let spans: String = (0..10_000)
        .map(|_| "<span style=\"color:#ff0000\">A</span><span style=\"color:#0000ff\">B</span>")
        .collect();
    let rich = vec![format!("(<p>{spans}</p>)")];
    shared_once("", &rich, |_| {
        "/Subtype /FreeText /BS << /W 0 >> /RC 4 0 R /Rect [10 10 110 40] \
         /DA (/Helv 12 Tf 0 g)"
            .to_owned()
    });
    shared_once("", &rich, |_| {
        "/Subtype /Widget /FT /Tx /Ff 33554432 /RV 4 0 R /Rect [10 10 110 40] \
         /DA (/Helv 12 Tf 0 g)"
            .to_owned()
    });
}

/// Text whose lines all lie above the page, however many it can make, is not laid
/// out: a `FreeText` and a multiline field two million points high, plain and rich.
#[test]
fn text_that_cannot_reach_the_page_is_not_laid_out() {
    let text = vec![format!("({})", long_text())];
    let rich = vec![format!("(<p>{}</p>)", long_text())];
    let tall = "/Rect [10 -1000000 110 1000000] /DA (/Helv 12 Tf 0 g)";
    shared_once("", &text, |_| {
        format!("/Subtype /FreeText /BS << /W 0 >> /Contents 4 0 R {tall}")
    });
    shared_once("", &rich, |_| {
        format!("/Subtype /FreeText /BS << /W 0 >> /RC 4 0 R {tall}")
    });
    shared_once("", &text, |_| {
        format!("/Subtype /Widget /FT /Tx /Ff 4096 /V 4 0 R {tall}")
    });
    shared_once("", &rich, |_| {
        format!("/Subtype /Widget /FT /Tx /Ff 33554432 /RV 4 0 R {tall}")
    });
}

/// A list whose options are drawn 0 high, as with a default appearance size of 0
/// and a default style that sets no size, draws none and looks at none: lists
/// sharing 20,000 options cost what one without options does. The labels a list
/// looks at are set before and cost no work, so the bytes tell.
#[test]
fn a_list_drawn_0_high_looks_at_no_option() {
    let options = vec![format!(
        "[{}]",
        (0..20_000).map(|i| format!("(o{i}) ")).collect::<String>()
    )];
    shared_once("", &options, |_| {
        "/Subtype /Widget /FT /Ch /Opt 4 0 R /DS (color:#ff0000) /Rect [10 10 110 60] \
         /MK << /BG [1 1 0] >> /DA (/Helv 0 Tf 0 g)"
            .to_owned()
    });
}

/// A kind of annotation measured, by name, and the entries of each.
type Kind = (&'static str, Box<dyn Fn(usize) -> String>);

/// Not a check but a measure, of what is left open: text whose box starts far
/// above the page and reaches it is laid out from its first line for each
/// annotation, down to the lines the page shows, as greedy line breaking at one
/// width tells nothing of the lines at another. Prints the work and bytes of `n`
/// `FreeText`s and multiline fields sharing a text of 3,636 words, laid out in some
/// thousand lines above the page, each at a width of its own or all at one, and,
/// for comparison, with the text's top on the page. `HIDDEN_PREFIX_N` runs one `n`
/// and kind alone (`HIDDEN_PREFIX_KIND`: 0–5 in the order printed), for counting
/// instructions with `perf stat`.
#[test]
#[ignore = "a measure, run by hand"]
fn hidden_prefix_cost() {
    let text = vec![format!("({})", long_text())];
    let thousandths = |i: usize| format!("{}.{:03}", 100 + i / 1000, i % 1000);
    let kinds: [Kind; 6] = [
        (
            "FreeText, widths of their own, top 15000",
            Box::new(move |i| {
                format!(
                    "/Subtype /FreeText /BS << /W 0 >> /Contents 4 0 R /Rect [10 0 {} 15000] \
                     /DA (/Helv 12 Tf 0 g)",
                    thousandths(i)
                )
            }),
        ),
        (
            "FreeText, one width, top 15000",
            Box::new(|_| {
                "/Subtype /FreeText /BS << /W 0 >> /Contents 4 0 R /Rect [10 0 110 15000] \
                 /DA (/Helv 12 Tf 0 g)"
                    .to_owned()
            }),
        ),
        (
            "FreeText, widths of their own, top 200",
            Box::new(move |i| {
                format!(
                    "/Subtype /FreeText /BS << /W 0 >> /Contents 4 0 R /Rect [10 0 {} 200] \
                     /DA (/Helv 12 Tf 0 g)",
                    thousandths(i)
                )
            }),
        ),
        (
            "multiline field, widths of their own, top 15000",
            Box::new(move |i| {
                format!(
                    "/Subtype /Widget /FT /Tx /Ff 4096 /V 4 0 R /Rect [10 0 {} 15000] \
                     /DA (/Helv 12 Tf 0 g)",
                    thousandths(i)
                )
            }),
        ),
        (
            "multiline field, one width, top 15000",
            Box::new(|_| {
                "/Subtype /Widget /FT /Tx /Ff 4096 /V 4 0 R /Rect [10 0 110 15000] \
                 /DA (/Helv 12 Tf 0 g)"
                    .to_owned()
            }),
        ),
        (
            "multiline field, widths of their own, top 200",
            Box::new(move |i| {
                format!(
                    "/Subtype /Widget /FT /Tx /Ff 4096 /V 4 0 R /Rect [10 0 {} 200] \
                     /DA (/Helv 12 Tf 0 g)",
                    thousandths(i)
                )
            }),
        ),
    ];
    let alone = std::env::var("HIDDEN_PREFIX_N")
        .ok()
        .and_then(|n| n.parse::<usize>().ok());
    if let Some(n) = alone {
        let kind: usize = std::env::var("HIDDEN_PREFIX_KIND")
            .ok()
            .and_then(|k| k.parse().ok())
            .unwrap_or(0);
        let cost = cost_of(n, "", &text, &*kinds[kind].1);
        println!("{} n={n}: {cost:?}", kinds[kind].0);
        return;
    }
    for (name, annot) in &kinds {
        let costs: Vec<Cost> = [40, 80, 160]
            .iter()
            .map(|&n| cost_of(n, "", &text, &**annot))
            .collect();
        println!(
            "{name}: 40 {:?}, 80 {:?}, 160 {:?}; per added annotation: work {} {}, bytes {} {}",
            costs[0],
            costs[1],
            costs[2],
            (costs[1].work - costs[0].work) / 40,
            (costs[2].work - costs[1].work) / 80,
            (costs[1].bytes - costs[0].bytes) / 40,
            (costs[2].bytes - costs[1].bytes) / 80,
        );
    }
}

/// What a page draws, counted: appearance streams, generated appearances, and
/// annotations that cannot be drawn.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Drew {
    streams: usize,
    generated: usize,
    unsupported: usize,
}

/// What reading a page's annotations and looking at them costs: bytes allocated,
/// units of work, bytes the file's readers looked at, and seconds.
#[derive(Debug, Clone, Copy)]
struct Listed {
    bytes: usize,
    work: u64,
    examined: usize,
    secs: f64,
    drew: Drew,
    /// The bytes held for the document once the annotations are looked at.
    kept: isize,
}

/// The bytes the readers of this thread have looked at so far
/// ([`hayro_interpret::hayro_syntax::work::EXAMINED`]).
fn examined() -> usize {
    work::EXAMINED.get()
}

/// The cost of `f`, given the appearances of `pdf`'s annotations, made inside.
fn listed<'a>(pdf: &'a Pdf, f: impl FnOnce(&Appearances<'a>) -> Drew) -> Listed {
    let (bytes, work, looked, start, kept) = (
        allocated(),
        work_done(),
        examined(),
        std::time::Instant::now(),
        held(),
    );
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let drew = f(&appearances);
    Listed {
        bytes: allocated() - bytes,
        work: work_done() - work,
        examined: examined() - looked,
        secs: start.elapsed().as_secs_f64(),
        drew,
        kept: held() - kept,
    }
}

/// Asserts that `more`, 40 more references than `fewer` to what it shares, cost
/// `bytes` and `work` at most for each, and no time that reading what they share
/// again takes: a few hundredths of a second for 40 reads of a million bytes.
fn costs_at_most(fewer: Listed, more: Listed, bytes: usize, work: u64, what: &str) {
    let (b, w, e, s) = (
        more.bytes.saturating_sub(fewer.bytes),
        more.work.saturating_sub(fewer.work),
        more.examined.saturating_sub(fewer.examined),
        more.secs - fewer.secs,
    );
    if std::env::var_os("COST_REPORT").is_some() {
        println!("CASE {what}: +40: {b} bytes, {w} work, {e} examined, {s:.4} s");
    }
    assert!(
        b < 40 * bytes && w < 40 * work && s < 0.25,
        "{what}: {fewer:?} for 40, {more:?} for 80"
    );
}

/// A long escaped name, 60,000 bytes as written and 20,000 read.
fn long_name() -> String {
    format!("/{}", "#41".repeat(20_000))
}

/// A number of a million digits, `n` after leading zeros.
fn long_number(n: u32) -> String {
    format!("{}{n}", "0".repeat(1_000_000))
}

/// The cost of a page whose `/Annots` names one annotation, `annot`, `n` times,
/// from reading its `/Annots` to each look, as viewed.
fn named_cost(n: usize, annot: &str) -> Listed {
    let pdf = Pdf::new(common::one_page(
        200,
        &format!("/Annots [{}]", vec!["4 0 R"; n].join(" ")),
        vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
    ))
    .expect("valid pdf");
    let page = &pdf.pages()[0];
    listed(&pdf, |appearances| drew(appearances, page, PAGE))
}

/// An annotation a page names again costs a lookup each time, from its entry in
/// `/Annots` on: it is not read, selected or looked at again, whatever it holds
/// and whatever its look comes to: drawn with a long entry it does not use, hidden
/// by long flags, drawn with long contents that are no text, of a long unknown
/// subtype, with an appearance that is a long name or string, drawn from long rich
/// text it holds.
#[test]
fn an_annotation_named_again_costs_a_lookup() {
    let unused = format!(
        "/Subtype /Square /C [0] /Rect [10 10 110 40] {} [{}]",
        long_name(),
        "0 ".repeat(20_000)
    );
    let hidden = format!(
        "/Subtype /Square /C [0] /Rect [10 10 110 40] /F {}",
        long_number(2)
    );
    let contents = format!(
        "/Subtype /FreeText /Rect [10 10 110 40] /DA (/Helv 12 Tf 0 g) /Contents {}",
        long_number(5)
    );
    let subtype = format!("/Subtype {} /Rect [10 10 110 40]", long_name());
    let ap = format!(
        "/Subtype /Square /C [0] /Rect [10 10 110 40] /AP {}",
        long_name()
    );
    let ap_string = format!(
        "/Subtype /Square /Rect [10 10 110 40] /C [1 0 0] /AP ({})",
        "ABCD\\)efg".repeat(2_222)
    );
    // Rich text written in the annotation, one word whose start alone its box shows.
    let word = "ABCD\\)efg".repeat(2_222);
    let rc = format!(
        "/Subtype /FreeText /BS << /W 0 >> /RC (<p>{word}</p>) /Rect [10 10 110 40] \
         /DA (/Helv 12 Tf 0 g)"
    );
    let rv = format!(
        "/Subtype /Widget /FT /Tx /Ff 33554432 /RV (<p>{word}</p>) /Rect [10 10 110 40] \
         /DA (/Helv 12 Tf 0 g)"
    );
    for (annot, generated) in [
        (unused, 1),
        (hidden, 0),
        (contents, 1),
        (subtype, 0),
        (ap, 0),
        (ap_string, 0),
        (rc, 1),
        (rv, 1),
    ] {
        let (fewer, more) = (named_cost(40, &annot), named_cost(80, &annot));
        let what = &annot[..40];
        assert_eq!(fewer.drew.generated, 40 * generated, "{what}");
        assert_eq!(more.drew.generated, 80 * generated, "{what}");
        costs_at_most(fewer, more, 256, 8, what);
    }
}

/// The cost of a page whose `/Annots` names `n` annotations `annot`, each an object
/// of its own after `shared`, numbered from 4.
fn sharing_cost(n: usize, shared: &[String], annot: &str) -> Listed {
    let first = 4 + shared.len();
    let refs: Vec<String> = (0..n).map(|i| format!("{} 0 R", first + i)).collect();
    let mut objects: Vec<Vec<u8>> = shared.iter().map(|s| s.clone().into_bytes()).collect();
    objects.extend((0..n).map(|_| format!("<< /Type /Annot {annot} >>").into_bytes()));
    let pdf = Pdf::new(common::one_page(
        200,
        &format!("/Annots [{}]", refs.join(" ")),
        objects,
    ))
    .expect("valid pdf");
    let page = &pdf.pages()[0];
    listed(&pdf, |appearances| drew(appearances, page, PAGE))
}

/// Annotations that share an object read it once, however they reach it: an
/// appearance stream whose `/BBox` is a long array of its own, read once for all
/// (its first entry a long name, read as 0), and a long subtype name, copied for
/// none.
#[test]
fn what_annotations_share_is_read_once_however_reached() {
    let bbox = vec![
        String::from_utf8(common::stream(
            "/Subtype /Form /BBox 5 0 R",
            b"0 0 m 10 10 l S",
        ))
        .unwrap(),
        format!("[{} 0 100 100 {}]", long_name(), "0 ".repeat(200_000)),
    ];
    let stream = "/Subtype /Square /Rect [10 10 110 40] /AP << /N 4 0 R >>";
    let (fewer, more) = (
        sharing_cost(40, &bbox, stream),
        sharing_cost(80, &bbox, stream),
    );
    assert_eq!(more.drew.streams, 80);
    // About 8.5 KiB an annotation, most of it the contexts its objects are read in,
    // whose chains of references name each body by 16 bytes.
    costs_at_most(fewer, more, 10 << 10, 1_000, "a shared /BBox");
    let name = vec![long_name()];
    let subtype = "/Subtype 4 0 R /Rect [10 10 110 40]";
    let (fewer, more) = (
        sharing_cost(40, &name, subtype),
        sharing_cost(80, &name, subtype),
    );
    costs_at_most(fewer, more, 8 << 10, 1_000, "a shared subtype");
}

/// A `FreeText` whose contents are a long number, no text, looked at on pages that
/// show it differently, reads its contents once: each look costs what it draws.
#[test]
fn an_annotation_shown_differently_reads_its_entries_once() {
    let annot = format!(
        "/Subtype /FreeText /Rect [10 10 110 40] /DA (/Helv 12 Tf 0 g) /Contents {}",
        long_number(5)
    );
    let pdf = Pdf::new(common::one_page(
        200,
        "/Annots [4 0 R]",
        vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
    ))
    .expect("valid pdf");
    let page = &pdf.pages()[0];
    let views = |n: usize| {
        listed(&pdf, |appearances| {
            let mut all = Drew::default();
            for i in 0..n {
                let view = kurbo::Rect::new(0.0, 0.0, 200.0 + i as f64 / 64.0, 200.0);
                all.generated += drew(appearances, page, view).generated;
            }
            all
        })
    };
    let (fewer, more) = (views(40), views(80));
    assert_eq!(more.drew.generated, 80);
    costs_at_most(fewer, more, 16 << 10, 1_000, "seen differently");
}

/// The cost of `n` pages whose `/Annots` is one array object, `4 0 R`: an
/// annotation written in it with a long unused entry, which draws nothing, and a
/// square of its own; each page shown as `view(i)` tells.
fn shared_annots_cost(n: usize, view: impl Fn(usize) -> kurbo::Rect) -> Listed {
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 6 + i)).collect();
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")).into_bytes(),
        b"<< >>".to_vec(),
        format!(
            "[<< /Type /Annot /Subtype /Square /C [0] /Rect [10 10 110 40] {} 0 >> 5 0 R]",
            long_name()
        )
        .into_bytes(),
        b"<< /Type /Annot /Subtype /Square /C [0] /Rect [10 10 110 40] >>".to_vec(),
    ];
    objects.extend((0..n).map(|_| {
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots 4 0 R >>".to_vec()
    }));
    let pdf = Pdf::new(common::file(&objects, "")).expect("valid pdf");
    let pages = pdf.pages();
    listed(&pdf, |appearances| {
        let mut all = Drew::default();
        for (i, page) in pages.iter().enumerate() {
            all.generated += drew(appearances, page, view(i)).generated;
        }
        all
    })
}

/// Pages that share their `/Annots` array read it once: each page shown as the
/// others costs a lookup, and each shown otherwise what it draws.
#[test]
fn pages_sharing_their_annotations_read_them_once() {
    let (fewer, more) = (
        shared_annots_cost(40, |_| PAGE),
        shared_annots_cost(80, |_| PAGE),
    );
    assert_eq!(more.drew.generated, 80);
    costs_at_most(fewer, more, 256, 8, "one view");
    let view = |i: usize| kurbo::Rect::new(0.0, 0.0, 200.0 + i as f64 / 64.0, 200.0);
    let (fewer, more) = (shared_annots_cost(40, view), shared_annots_cost(80, view));
    assert_eq!(more.drew.generated, 80);
    costs_at_most(fewer, more, 16 << 10, 1_000, "views of their own");
}

/// What `appearances` draws of `page` as viewed where `visible` shows, counted.
fn drew<'a>(appearances: &Appearances<'a>, page: &Page<'a>, visible: kurbo::Rect) -> Drew {
    let mut drew = Drew::default();
    for d in appearances
        .page(page, AnnotationSelection::View, &View::new(visible, 1.0))
        .iter()
    {
        match d.look() {
            Look::Stream { .. } => drew.streams += 1,
            Look::Generated(_) => drew.generated += 1,
            Look::Nothing => panic!("a page lists no annotation that draws nothing"),
            Look::Unsupported(_) => drew.unsupported += 1,
        }
    }
    drew
}

/// The cost of `n` looks at one page whose `/Annots` is written in it: an
/// annotation written there with a long unused entry, which draws nothing, and a
/// square of its own; look `i` shows `view(i)`.
fn inline_annots_cost(n: usize, view: impl Fn(usize) -> kurbo::Rect) -> Listed {
    let pdf = Pdf::new(common::one_page(
        200,
        &format!(
            "/Annots [<< /Type /Annot /Subtype /Square /C [0] /Rect [10 10 110 40] {} 0 >> 4 0 R]",
            long_name()
        ),
        vec![b"<< /Type /Annot /Subtype /Square /C [0] /Rect [10 10 110 40] >>".to_vec()],
    ))
    .expect("valid pdf");
    let page = &pdf.pages()[0];
    listed(&pdf, |appearances| {
        let mut all = Drew::default();
        for i in 0..n {
            all.generated += drew(appearances, page, view(i)).generated;
        }
        all
    })
}

/// A page whose `/Annots` is written in it, looked at again, costs a lookup, and,
/// shown otherwise, what it draws: the array and what is written in it are not
/// read again.
#[test]
fn a_page_looked_at_again_reads_its_written_annotations_once() {
    let (fewer, more) = (
        inline_annots_cost(40, |_| PAGE),
        inline_annots_cost(80, |_| PAGE),
    );
    assert_eq!(more.drew.generated, 80);
    costs_at_most(fewer, more, 256, 8, "one view");
    let view = |i: usize| kurbo::Rect::new(0.0, 0.0, 200.0 + i as f64 / 64.0, 200.0);
    let (fewer, more) = (inline_annots_cost(40, view), inline_annots_cost(80, view));
    assert_eq!(more.drew.generated, 80);
    costs_at_most(fewer, more, 16 << 10, 1_000, "views of their own");
}

/// The cost of a page with `n` annotations, each with an appearance stream of its
/// own, `5 0 m` of a box `bbox`, whose `/Length` is one shared object: a number of a
/// million digits.
fn shared_length_cost(n: usize, bbox: &str) -> Listed {
    let mut objects = vec![long_number(5).into_bytes()];
    for i in 0..n {
        objects.push(
            format!(
                "<< /Type /Annot /Subtype /Square /Rect [10 10 110 40] /AP << /N {} 0 R >> >>",
                5 + n + i
            )
            .into_bytes(),
        );
    }
    for _ in 0..n {
        objects.push(
            format!("<< /Subtype /Form /BBox {bbox} /Length 4 0 R >>\nstream\n5 0 m\nendstream")
                .into_bytes(),
        );
    }
    let refs: Vec<String> = (0..n).map(|i| format!("{} 0 R", 5 + i)).collect();
    let pdf = Pdf::new(common::one_page(
        200,
        &format!("/Annots [{}]", refs.join(" ")),
        objects,
    ))
    .expect("valid pdf");
    let page = &pdf.pages()[0];
    listed(&pdf, |appearances| drew(appearances, page, PAGE))
}

/// Appearance streams that share their `/Length`, an object, read it once: each
/// stream costs what its own dictionary holds, some 12 KiB allocated, from listing
/// it to placing it, whether it draws or its box draws nothing; reading the shared
/// million digits again would take a hundredth of a second a stream.
#[test]
fn streams_sharing_a_long_length_read_it_once() {
    for (bbox, streams) in [("[0 0 10 10]", 1), ("[0 0 0 0]", 0)] {
        let (fewer, more) = (shared_length_cost(40, bbox), shared_length_cost(80, bbox));
        assert_eq!(more.drew.streams, 80 * streams, "{bbox}");
        costs_at_most(fewer, more, 16 << 10, 1_000, bbox);
        assert!(
            more.examined - fewer.examined < 40 * 1_000,
            "{bbox}: {fewer:?} {more:?}"
        );
    }
}

/// The cost of a page with `n` annotations that share their state, `/AS 4 0 R`, a
/// name of `len` bytes, and their appearances, `/AP 5 0 R`, whose `/N` holds that
/// state as `state`.
fn shared_state_cost(n: usize, len: usize, state: &str) -> Listed {
    let name = format!("/{}", "A".repeat(len));
    let mut objects = vec![
        name.clone().into_bytes(),
        format!("<< /N << /Off 6 0 R {name} {state} >> >>").into_bytes(),
        common::stream("/Subtype /Form /BBox [0 0 0 0]", b"0 0 m 10 10 l S"),
    ];
    objects.extend((0..n).map(|_| {
        b"<< /Type /Annot /Subtype /Square /C [0] /Rect [10 10 110 40] /AS 4 0 R /AP 5 0 R >>"
            .to_vec()
    }));
    let refs: Vec<String> = (0..n).map(|i| format!("{} 0 R", 7 + i)).collect();
    let pdf = Pdf::new(common::one_page(
        200,
        &format!("/Annots [{}]", refs.join(" ")),
        objects,
    ))
    .expect("valid pdf");
    let page = &pdf.pages()[0];
    listed(&pdf, |appearances| drew(appearances, page, PAGE))
}

/// The cost of a page with `n` annotations whose states, `/AS`, each name a long
/// name of `len` bytes, object 4, through an entry of the table of its own that
/// leads to it ([`common::file_with_aliases`]), or, not `aliased`, all through `4 0 R`, and
/// share their appearances, `/AP 5 0 R`, whose `/N` holds that state as `state`.
fn aliased_state_cost(n: usize, len: usize, state: &str, aliased: bool) -> Listed {
    let name = format!("/{}", "A".repeat(len));
    let states = format!("<< /N << /Off 6 0 R {name} {state} >> >>").into_bytes();
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots 7 0 R >>".to_vec(),
        name.into_bytes(),
        states,
        common::stream("/Subtype /Form /BBox [0 0 10 10]", b"0 0 m 10 10 l S"),
    ];
    let first = 8 + n;
    let refs: Vec<String> = (0..n).map(|i| format!("{} 0 R", 8 + i)).collect();
    objects.push(format!("[{}]", refs.join(" ")).into_bytes());
    objects.extend((0..n).map(|i| {
        let state = if aliased { first + i } else { 4 };
        format!(
            "<< /Type /Annot /Subtype /Square /C [0] /Rect [10 10 110 40] /AS {state} 0 R /AP 5 0 R >>"
        )
        .into_bytes()
    }));
    let aliases = if aliased { vec![4; n] } else { vec![] };
    let pdf = Pdf::new(common::file_with_aliases(objects, &aliases)).expect("valid pdf");
    let xref = pdf.xref();
    for i in 0..aliases.len() {
        let alias = ObjectIdentifier::new((first + i) as i32, 0);
        assert!(xref.content_key(alias).is_some());
        assert_eq!(
            xref.content_key(alias),
            xref.content_key(ObjectIdentifier::new(4, 0))
        );
    }
    let page = &pdf.pages()[0];
    listed(&pdf, |appearances| drew(appearances, page, PAGE))
}

/// Annotations whose states are one long name reached through entries of the table
/// of their own, which a damaged table keeps, draw what those that name it through
/// one entry draw, and find it once: each costs a lookup, whatever the name's
/// length, for a state that is a stream that draws and one that is no stream.
/// Hashing and comparing the name for each, which no counter sees, would take a
/// quarter of a second or more for 200 more of 8 MB.
#[test]
fn annotations_whose_states_are_one_name_through_many_entries_find_it_once() {
    for state in ["6 0 R", "1"] {
        let len = 8 << 20;
        let one = aliased_state_cost(240, len, state, false);
        let (fewer, more) = (
            aliased_state_cost(40, len, state, true),
            aliased_state_cost(240, len, state, true),
        );
        assert_eq!(more.drew, one.drew, "{state}");
        let (bytes, work, looked, secs) = (
            more.bytes - fewer.bytes,
            more.work - fewer.work,
            more.examined - fewer.examined,
            more.secs - fewer.secs,
        );
        if std::env::var_os("COST_REPORT").is_some() {
            println!(
                "CASE aliased {state}: +200: {bytes} bytes, {work} work, {looked} examined, {secs:.4} s"
            );
        }
        assert!(
            bytes < 200 * (16 << 10) && work < 200 * 1_000 && looked < 200 * 1_000 && secs < 0.1,
            "{state}: {fewer:?} {more:?}"
        );
    }
}

/// Annotations that share a state, a long name, and the states it names one of,
/// find what it draws once: each costs a lookup, whatever the name's length, for a
/// state that is a stream that draws nothing and one that is no stream. No counter
/// sees hashing and comparing the name, so the time tells: 200 more annotations
/// that each hashed and compared a name of 8 MB would take a quarter of a second or
/// more, where 200 lookups take a few milliseconds.
#[test]
fn annotations_sharing_a_long_state_find_it_once() {
    for state in ["6 0 R", "1"] {
        let len = 8 << 20;
        let (fewer, more) = (
            shared_state_cost(200, len, state),
            shared_state_cost(400, len, state),
        );
        assert_eq!(more.drew, Drew::default(), "{state}");
        let (bytes, work, looked, secs) = (
            more.bytes - fewer.bytes,
            more.work - fewer.work,
            more.examined - fewer.examined,
            more.secs - fewer.secs,
        );
        if std::env::var_os("COST_REPORT").is_some() {
            println!(
                "CASE {state}: +200: {bytes} bytes, {work} work, {looked} examined, {secs:.4} s"
            );
        }
        assert!(
            bytes < 200 * (16 << 10) && work < 200 * 1_000 && looked < 200 * 1_000 && secs < 0.1,
            "{state}: {fewer:?} {more:?}"
        );
    }
}

/// Distinct annotations that share a long array, an object of its own, walk it once
/// for the document, or as far as each draws it: each costs what it draws, however
/// long the array, where what it draws does not grow with the array. Cases: `/LE`
/// of more than two entries, which ends nothing, of a line drawn twice and of a
/// polyline; `/QuadPoints` of no area, of a highlight; quads of no height after one
/// that has, of a squiggly, which draws only the one; an `/InkList` of no path;
/// `/Vertices` of a polygon without colours, and of a polyline without a stroke
/// colour, which fills only its endings; a dash of a line that draws nothing, with
/// a leader line and no length, and of a line without colours.
#[test]
fn an_array_annotations_share_is_walked_once() {
    let long =
        |head: &str, entry: &str| vec![format!("[{head} {}]", vec![entry; 20_000].join(" "))];
    let quad = "10 30 100 30 10 10 100 10";
    let cases = [
        (
            long("", "/Square"),
            "/Subtype /Line /L [10 10 100 100] /C [1 0 0] /LE 4 0 R",
            1,
        ),
        (
            long("", "/Square"),
            "/Subtype /PolyLine /Vertices [10 10 100 100] /C [1 0 0] /LE 4 0 R",
            1,
        ),
        (
            long("", "0"),
            "/Subtype /Highlight /Rect [0 0 200 200] /QuadPoints 4 0 R",
            0,
        ),
        (
            long(quad, "0"),
            "/Subtype /Squiggly /Rect [0 0 200 200] /QuadPoints 4 0 R",
            1,
        ),
        (long("", "null"), "/Subtype /Ink /C [0] /InkList 4 0 R", 0),
        (long("", "50"), "/Subtype /Polygon /Vertices 4 0 R", 0),
        (
            long("10 10 100 10", "50"),
            "/Subtype /PolyLine /IC [1 0 0] /LE [/Square /Square] /Vertices 4 0 R",
            1,
        ),
        (
            long("", "3"),
            "/Subtype /Line /L [10 10 10 10] /LL 5 /C [0] /BS << /D 4 0 R >>",
            0,
        ),
        (
            long("", "3"),
            "/Subtype /Line /L [10 10 100 100] /BS << /D 4 0 R >>",
            0,
        ),
    ];
    for (shared, annot, generated) in cases {
        let (fewer, more) = (
            sharing_cost(40, &shared, annot),
            sharing_cost(80, &shared, annot),
        );
        assert_eq!(more.drew.generated, 80 * generated, "{annot}");
        assert_eq!(more.drew.unsupported, 0, "{annot}");
        costs_at_most(fewer, more, 20 << 10, 1_000, annot);
    }
}

/// What deciding whether an annotation may paint on the page reads of a payload
/// it shares with others, an object of its own, does not grow with that payload:
/// each annotation added costs the same, in bytes, work and bytes looked at,
/// whether the payload is short or millions of bytes long, and the output is the
/// same. Cases: lines off the page sharing `/Contents`, without a caption and with
/// one, whose length alone bounds where it is drawn; a line on the page sharing
/// it, without a caption, which does not read it; highlights and redactions off
/// the page sharing `/QuadPoints` of quads of some area, off the page too.
#[test]
fn a_shared_payload_costs_each_annotation_the_same_however_long() {
    const N: usize = 40;
    let contents = |n: usize| format!("({})", "A".repeat(n));
    let quads = |n: usize| {
        let quad = "1000 1010 1010 1010 1000 1000 1010 1000";
        format!("[{}]", vec![quad; n].join(" "))
    };
    let text = [20 << 10, 4 << 20];
    // About 40 and 1.6 MB.
    let many = [1_000, 40_000];
    type Case<'a> = (&'a dyn Fn(usize) -> String, [usize; 2], &'a str, usize);
    let cases: [Case<'_>; 6] = [
        (
            &contents,
            text,
            "/Subtype /Line /L [1000 1000 1100 1000] /C [0] /Contents 4 0 R",
            0,
        ),
        (
            &contents,
            text,
            "/Subtype /Line /L [1000 1000 1100 1000] /C [0] /Cap true /Contents 4 0 R",
            0,
        ),
        (
            &contents,
            text,
            "/Subtype /Line /L [10 10 100 10] /C [0] /Contents 4 0 R",
            1,
        ),
        (
            &quads,
            many,
            "/Subtype /Highlight /Rect [0 0 200 200] /C [1 1 0] /QuadPoints 4 0 R",
            0,
        ),
        (
            &quads,
            many,
            "/Subtype /Redact /Rect [1000 1000 1010 1010] /QuadPoints 4 0 R",
            0,
        ),
        (
            &quads,
            many,
            "/Subtype /Redact /Rect [0 0 200 200] /QuadPoints 4 0 R",
            0,
        ),
    ];
    for (payload, lengths, annot, generated) in cases {
        let added = |n: usize| {
            let shared = [payload(n)];
            let (one, two) = (
                sharing_cost(N, &shared, annot),
                sharing_cost(2 * N, &shared, annot),
            );
            assert_eq!(one.drew.generated, N * generated, "{annot}");
            assert_eq!(two.drew.generated, 2 * N * generated, "{annot}");
            assert_eq!(two.drew.unsupported, 0, "{annot}");
            (
                two.bytes.saturating_sub(one.bytes),
                two.work.saturating_sub(one.work),
                two.examined.saturating_sub(one.examined),
            )
        };
        let (short, long) = (added(lengths[0]), added(lengths[1]));
        if std::env::var_os("COST_REPORT").is_some() {
            println!(
                "CASE {annot}: +{N}: {short:?} at {}, {long:?} at {}",
                lengths[0], lengths[1]
            );
        }
        // The same but for the allocator rounding what it grows the same tables to.
        assert!(
            long.0 < N * (16 << 10)
                && long.0 <= short.0 + N * 64
                && long.1 == short.1
                && long.2 == short.2,
            "{annot}: {short:?} added at {}, {long:?} at {}",
            lengths[0],
            lengths[1]
        );
    }
}

/// The most more units of work `n` annotations each finding parts once in a tree
/// of `long` parts take than in one of `short` ([`hayro_interpret::annotation`]'s
/// index): a tree twice as large is one level deeper, where a search looks at
/// two more nodes on the way down to each part found.
fn deeper(n: usize, short: usize, long: usize) -> u64 {
    let levels = (long as f64 / short as f64).log2().ceil() as u64;
    (n as u64) * 2 * levels
}

/// Annotations that share geometry of which a part may paint on the page and the
/// rest may not each write the part that may, and walk none of the rest: each
/// annotation added costs the same, in bytes and bytes looked at, and in work but
/// for the search through a deeper tree, whether the part off the page is short or
/// long, and draws what it draws with that part one piece long. Cases: an `/InkList` of a short path on the page and a long one
/// off it; `/QuadPoints` of a quad on the page between many off it, of each text
/// markup subtype and of a redaction.
#[test]
fn shared_geometry_costs_each_annotation_what_it_draws() {
    const N: usize = 40;
    let ink = |n: usize| format!("[[20 20 80 80] [{}]]", "1000 1000 ".repeat(n));
    let visible = "20 60 120 60 20 40 120 40";
    let quads = |n: usize| {
        let off = vec!["1000 1010 1010 1010 1000 1000 1010 1000"; n].join(" ");
        format!("[{off} {visible} {off}]")
    };
    let many = [1_000, 40_000];
    type Case<'a> = (&'a dyn Fn(usize) -> String, &'a str);
    let cases: [Case<'_>; 6] = [
        (&ink, "/Subtype /Ink /C [1 0 0] /BS << /W 1 >> /InkList"),
        (&quads, "/Subtype /Highlight /C [1 1 0] /QuadPoints"),
        (&quads, "/Subtype /Underline /C [0] /QuadPoints"),
        (&quads, "/Subtype /StrikeOut /C [0] /QuadPoints"),
        (&quads, "/Subtype /Squiggly /C [0] /QuadPoints"),
        (&quads, "/Subtype /Redact /QuadPoints"),
    ];
    for (payload, annot) in cases {
        let shared_annot = format!("{annot} 4 0 R /Rect [0 0 200 200]");
        let added = |n: usize| {
            let shared = [payload(n)];
            let (one, two) = (
                sharing_cost(N, &shared, &shared_annot),
                sharing_cost(2 * N, &shared, &shared_annot),
            );
            assert_eq!(one.drew.generated, N, "{annot}");
            assert_eq!(two.drew.generated, 2 * N, "{annot}");
            (
                two.bytes.saturating_sub(one.bytes),
                two.work.saturating_sub(one.work),
                two.examined.saturating_sub(one.examined),
            )
        };
        let (short, long) = (added(many[0]), added(many[1]));
        if std::env::var_os("COST_REPORT").is_some() {
            println!(
                "CASE {annot}: +{N}: {short:?} at {}, {long:?} at {}",
                many[0], many[1]
            );
        }
        // At most what drawing a zigzag costs, the same but for the allocator
        // rounding what it grows the same tables to, and for the search through
        // a deeper tree.
        assert!(
            long.0 < N * (32 << 10)
                && long.0 <= short.0 + N * 64
                && long.1 <= short.1 + deeper(N, 2 * many[0], 2 * many[1])
                && long.2 == short.2,
            "{annot}: {short:?} added at {}, {long:?} at {}",
            many[0],
            many[1]
        );
        // What is drawn is what is drawn with one piece off the page, which clips
        // the quads alike.
        let alone = generated(&format!("{annot} {} /Rect [0 0 200 200]", payload(1)));
        for n in many {
            let mixed = generated(&format!("{annot} {} /Rect [0 0 200 200]", payload(n)));
            assert_eq!(mixed, alone, "{annot} at {n}");
        }
    }
}

/// Ink annotations of widths each its own sharing an `/InkList` of many short paths
/// off the page and one on it find the paths their stroke reaches by a search:
/// each annotation added costs the same in bytes, and in work but for the search
/// through a deeper tree, whether the list holds a thousand paths or forty
/// thousand, and draws the path on the page alone.
#[test]
fn ink_of_widths_each_its_own_finds_the_paths_it_reaches() {
    const N: usize = 40;
    let ink = |n: usize| format!("[[20 20 80 80] {}]", "[1000 1000 1001 1001] ".repeat(n));
    let annot = |i: usize| {
        format!("/Subtype /Ink /C [1 0 0] /BS << /W 1.{i:04} >> /InkList 4 0 R /Rect [0 0 200 200]")
    };
    let added = |n: usize| {
        let shared = [ink(n)];
        let (one, two) = (
            cost_of(N, "", &shared, &annot),
            cost_of(2 * N, "", &shared, &annot),
        );
        (two.bytes.saturating_sub(one.bytes), two.work - one.work)
    };
    let (short, long) = (added(1_000), added(40_000));
    if std::env::var_os("COST_REPORT").is_some() {
        println!("CASE ink of own widths: +{N}: {short:?} at 1000, {long:?} at 40000");
    }
    assert!(
        long.0 < N * (32 << 10)
            && long.0 <= short.0 + N * 64
            && long.1 <= short.1 + deeper(N, 1_000, 40_000),
        "{short:?} added at 1,000, {long:?} at 40,000"
    );
    let content = generated(&annot(7).replace("4 0 R", &ink(1_000)));
    assert_eq!(content.matches(" m\n").count(), 1, "{content}");
}

/// Ink annotations sharing an `/InkList` of one path on the page and many just
/// beyond the reach of their stroke, nearer than a search to within a fraction of
/// the reach tells apart, each take the paths their stroke reaches by one search
/// and test none of the rest: each annotation added costs the same in bytes, and
/// in work but for the search through a deeper tree, whether the list holds a
/// thousand such paths or forty thousand, of one width or of widths each its own
/// a hair apart, and draws the path on the page alone. The paths lie above the page, whose edge a 1 pt stroke
/// reaches to within 5 pt, and to its right, with a point at the edge of what a
/// 32-bit number can tell.
#[test]
fn ink_paths_just_beyond_the_reach_cost_each_annotation_nothing() {
    const N: usize = 40;
    let beyond = [
        "[50 205.2516 51 206.2516]",
        "[205.25152587890625 20 206.25152587890625 21]",
    ];
    let widths: [fn(usize) -> String; 2] = [|_| String::from("1"), |i| format!("1.{i:07}")];
    for path in beyond {
        for width in widths {
            let ink = |n: usize| format!("[[20 20 80 80] {}]", format!("{path} ").repeat(n));
            let annot = |i: usize| {
                format!(
                    "/Subtype /Ink /C [1 0 0] /BS << /W {} >> /InkList 4 0 R \
                     /Rect [0 0 200 200]",
                    width(i)
                )
            };
            let added = |n: usize| {
                let shared = [ink(n)];
                let (one, two) = (
                    cost_of(N, "", &shared, &annot),
                    cost_of(2 * N, "", &shared, &annot),
                );
                (two.bytes.saturating_sub(one.bytes), two.work - one.work)
            };
            let (short, long) = (added(1_000), added(40_000));
            if std::env::var_os("COST_REPORT").is_some() {
                println!(
                    "CASE ink beyond {path} {}: +{N}: {short:?} at 1000, {long:?} at 40000",
                    width(1)
                );
            }
            assert!(
                long.0 < N * (32 << 10)
                    && long.0 <= short.0 + N * 64
                    && long.1 <= short.1 + deeper(N, 1_000, 40_000),
                "{path} {}: {short:?} added at 1,000, {long:?} at 40,000",
                width(1)
            );
            for i in [0, 2 * N - 1] {
                let content = generated(&annot(i).replace("4 0 R", &ink(1_000)));
                assert_eq!(content.matches(" m\n").count(), 1, "{content}");
            }
        }
    }
}

/// Ink annotations sharing a long dash array, whose paths cross the page as a
/// whole but each lie off it, draw nothing, and cost the same, in bytes, work and
/// bytes looked at, whether the array holds a thousand entries or forty thousand:
/// the pattern is made once for the document and held as it is until something
/// paints. Where a path paints, the pattern is written whole.
#[test]
fn a_shared_dash_costs_nothing_where_nothing_paints() {
    const N: usize = 40;
    let dash = |n: usize| format!("[{}]", "3 ".repeat(n));
    let annot = |paths: &str| {
        format!(
            "/Subtype /Ink /C [1 0 0] /BS << /W 1 /D 4 0 R >> /InkList {paths} \
             /Rect [0 0 200 200]"
        )
    };
    let off = annot("[[-1000 -1000 -999 -999] [1000 1000 1001 1001]]");
    let added = |n: usize| {
        let shared = [dash(n)];
        let (one, two) = (
            sharing_cost(N, &shared, &off),
            sharing_cost(2 * N, &shared, &off),
        );
        assert_eq!((one.drew.generated, two.drew.generated), (0, 0));
        (
            two.bytes.saturating_sub(one.bytes),
            two.work.saturating_sub(one.work),
            two.examined.saturating_sub(one.examined),
        )
    };
    let (short, long) = (added(1_000), added(40_000));
    if std::env::var_os("COST_REPORT").is_some() {
        println!("CASE shared dash, nothing painted: +{N}: {short:?} at 1000, {long:?} at 40000");
    }
    assert!(
        long.0 < N * (32 << 10)
            && long.0 <= short.0 + N * 64
            && long.1 == short.1
            && long.2 == short.2,
        "{short:?} added at 1,000, {long:?} at 40,000"
    );
    for n in [1_000, 40_000] {
        let inline = |paths: &str| annot(paths).replace("4 0 R", &dash(n));
        let off = inline("[[-1000 -1000 -999 -999] [1000 1000 1001 1001]]");
        assert!(
            matches!(first_look(&one_annotation(&off)), Look::Nothing),
            "{n}"
        );
        let content = generated(&inline("[[-1000 -1000 -999 -999] [20 20 80 80]]"));
        assert_eq!(
            content,
            format!(
                "1 0 0 RG\n0 J 0 j\n10 M\n1 w\n[{}] 0 d\n1 J 1 j\n20 20 m\n80 80 l\nS\n",
                "3 ".repeat(n)
            )
        );
    }
}

/// The parts of shared geometry that may paint, found in one tree of each kind of
/// part for every view and width, are those of the view, the width and the kind:
/// annotations sharing an `/InkList`, of widths that reach the part off the page
/// or not, and annotations of each text markup subtype and a redaction sharing
/// `/QuadPoints`, viewed one view after another by one [`Appearances`], each look
/// as when viewed alone.
#[test]
fn the_parts_that_may_paint_are_told_for_each_view_width_and_kind() {
    let ink = "[[20 20 80 80] [1000 1000 1010 1000]]";
    let quads = "[20 60 120 60 20 40 120 40 1000 1010 1010 1010 1000 1000 1010 1000]";
    let annots = [
        "/Subtype /Ink /C [1 0 0] /BS << /W 1 >> /InkList 4 0 R",
        "/Subtype /Ink /C [1 0 0] /BS << /W 400 >> /InkList 4 0 R",
        "/Subtype /Highlight /C [1 1 0] /QuadPoints 5 0 R",
        "/Subtype /Underline /C [0] /QuadPoints 5 0 R",
        "/Subtype /StrikeOut /C [0] /QuadPoints 5 0 R",
        "/Subtype /Squiggly /C [0] /QuadPoints 5 0 R",
        "/Subtype /Redact /QuadPoints 5 0 R",
    ];
    // The annotations sharing the arrays, or each holding its own.
    let file = |shared: bool| {
        let refs: Vec<String> = (0..annots.len())
            .map(|i| format!("{} 0 R", 6 + i))
            .collect();
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [{}] >>",
                refs.join(" ")
            )
            .into_bytes(),
            ink.as_bytes().to_vec(),
            quads.as_bytes().to_vec(),
        ];
        objects.extend(annots.iter().map(|a| {
            let a = if shared {
                a.to_string()
            } else {
                a.replace("4 0 R", ink).replace("5 0 R", quads)
            };
            format!("<< /Type /Annot {a} /Rect [0 0 200 200] >>").into_bytes()
        }));
        Pdf::new(common::file(&objects, "")).expect("valid pdf")
    };
    fn looks<'a>(appearances: &Appearances<'a>, page: &Page<'a>, view: kurbo::Rect) -> Vec<String> {
        let drawn = appearances.page(page, AnnotationSelection::View, &View::new(view, 1.0));
        drawn
            .iter()
            .map(|d| match d.look() {
                Look::Generated(g) => format!("{}: {}", d.index(), g.content),
                Look::Unsupported(why) => format!("{}: {why:?}", d.index()),
                _ => format!("{}: stream or nothing", d.index()),
            })
            .collect()
    }
    let views = [
        PAGE,
        kurbo::Rect::new(990.0, 990.0, 1020.0, 1020.0),
        kurbo::Rect::new(-500.0, -500.0, 2000.0, 2000.0),
        kurbo::Rect::new(300.0, 300.0, 500.0, 500.0),
    ];
    let font_resolver = || InterpreterSettings::default().font_resolver;
    let (shared, own) = (file(true), file(false));
    let (page, own_page) = (&shared.pages()[0], &own.pages()[0]);
    let one = Appearances::new(shared.xref(), font_resolver());
    let mut drew = 0;
    for view in views.iter().chain(views.iter().rev()) {
        let alone = looks(
            &Appearances::new(own.xref(), font_resolver()),
            own_page,
            *view,
        );
        assert_eq!(looks(&one, page, *view), alone, "{view:?}");
        drew += alone.len();
    }
    assert!(drew > 2 * annots.len(), "{drew} looks");
}

/// Ink annotations each with an `/InkList` of its own that refers to one long
/// path they share, off the page, walk the path once between them: each
/// annotation added costs the same, in bytes allocated and held, work and bytes
/// looked at, whether the path holds a thousand points or forty thousand, both
/// where the list holds that path alone and nothing is drawn, and where it holds
/// a short path on the page too, which is drawn alone.
#[test]
fn ink_lists_of_their_own_sharing_a_path_walk_it_once() {
    const N: usize = 40;
    let path = |n: usize| format!("[{}]", "1000 1000 ".repeat(n));
    for (list, drawn) in [("[4 0 R]", 0), ("[4 0 R 5 0 R]", 1)] {
        let annot =
            format!("/Subtype /Ink /C [1 0 0] /BS << /W 1 >> /InkList {list} /Rect [0 0 200 200]");
        let added = |n: usize| {
            let shared = [path(n), String::from("[20 20 80 80]")];
            let (one, two) = (
                sharing_cost(N, &shared, &annot),
                sharing_cost(2 * N, &shared, &annot),
            );
            assert_eq!(one.drew.generated, N * drawn, "{list}");
            assert_eq!(two.drew.generated, 2 * N * drawn, "{list}");
            (
                two.bytes.saturating_sub(one.bytes),
                two.work.saturating_sub(one.work),
                two.examined.saturating_sub(one.examined),
                two.kept - one.kept,
            )
        };
        let (short, long) = (added(1_000), added(40_000));
        if std::env::var_os("COST_REPORT").is_some() {
            println!(
                "CASE ink lists of their own {list}: +{N}: {short:?} at 1000, {long:?} at 40000"
            );
        }
        assert!(
            long.0 < N * (32 << 10)
                && long.0 <= short.0 + N * 64
                && long.1 == short.1
                && long.2 == short.2
                && long.3 <= short.3 + (N * 64) as isize,
            "{list}: {short:?} added at 1,000, {long:?} at 40,000"
        );
        if drawn == 1 {
            let own = annot.replace("4 0 R 5 0 R", &format!("{} [20 20 80 80]", path(1_000)));
            assert_eq!(
                generated(&own),
                "1 0 0 RG\n0 J 0 j\n10 M\n1 w\n[] 0 d\n1 J 1 j\n20 20 m\n80 80 l\nS\n"
            );
        }
    }
}

/// One [`Appearances`] looking at many pages, each showing a box of its own, of
/// annotations they share, whose geometry is one short piece on the pages and many
/// off them, finds the pieces each page shows by searching one tree of them made
/// for the document: each page added costs the same, in bytes allocated and held,
/// whether the geometry holds a thousand pieces off the pages or forty thousand,
/// and in work but for the search through a deeper tree. Ink of two widths
/// sharing an `/InkList`, and an underline and a squiggly sharing `/QuadPoints`.
#[test]
fn pages_of_their_own_views_find_shared_pieces_in_one_tree() {
    const ANNOTS: usize = 4;
    let file = |pages: usize, n: usize| {
        let first = 4 + 2 + ANNOTS;
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", first + i)).collect();
        let off = "1000 1010 1010 1010 1000 1000 1010 1000 ".repeat(n);
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {pages} >>",
                kids.join(" ")
            )
            .into_bytes(),
            b"null".to_vec(),
            format!("[[20 20 80 80] {}]", "[1000 1000 1001 1001] ".repeat(n)).into_bytes(),
            format!("[20 60 120 60 20 40 120 40 {off}]").into_bytes(),
        ];
        for annot in [
            "/Subtype /Ink /C [1 0 0] /BS << /W 1 >> /InkList 4 0 R",
            "/Subtype /Ink /C [1 0 0] /BS << /W 2 >> /InkList 4 0 R",
            "/Subtype /Underline /C [0] /QuadPoints 5 0 R",
            "/Subtype /Squiggly /C [0] /QuadPoints 5 0 R",
        ] {
            objects.push(format!("<< /Type /Annot {annot} /Rect [0 0 200 200] >>").into_bytes());
        }
        let annots: Vec<String> = (0..ANNOTS).map(|i| format!("{} 0 R", 6 + i)).collect();
        objects.extend((0..pages).map(|i| {
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /CropBox [0 0 {} 200] /Annots [{}] >>",
                199.0 - i as f64 * 0.01,
                annots.join(" ")
            )
            .into_bytes()
        }));
        Pdf::new(common::file(&objects, "")).expect("valid pdf")
    };
    let cost = |pages: usize, n: usize| {
        let pdf = file(pages, n);
        listed(&pdf, |appearances| {
            let mut drew = Drew::default();
            for page in pdf.pages().iter() {
                for d in appearances
                    .page(page, AnnotationSelection::View, &View::of(page))
                    .iter()
                {
                    match d.look() {
                        Look::Generated(_) => drew.generated += 1,
                        _ => drew.unsupported += 1,
                    }
                }
            }
            drew
        })
    };
    const PAGES: usize = 20;
    let added = |n: usize| {
        let (one, two) = (cost(PAGES, n), cost(2 * PAGES, n));
        assert_eq!(one.drew.generated, PAGES * ANNOTS, "{n}");
        assert_eq!(two.drew.generated, 2 * PAGES * ANNOTS, "{n}");
        assert_eq!(two.drew.unsupported, 0, "{n}");
        (
            two.bytes.saturating_sub(one.bytes),
            two.work.saturating_sub(one.work),
            two.kept - one.kept,
        )
    };
    let (short, long) = (added(1_000), added(40_000));
    if std::env::var_os("COST_REPORT").is_some() {
        println!("CASE pages of their own views: +{PAGES}: {short:?} at 1000, {long:?} at 40000");
    }
    let looks = PAGES * ANNOTS;
    assert!(
        long.0 < looks * (16 << 10)
            && long.0 <= short.0 + looks * 64
            && long.1 <= short.1 + deeper(looks, 1_000, 40_000)
            && long.2 <= short.2 + (looks * 64) as isize,
        "{short:?} added at 1,000, {long:?} at 40,000"
    );
}

/// Stamps that share a long name read it once for the document, and set it once:
/// those of no area, which draw nothing; those whose name is spaces, which draw
/// their frame alone; and those whose name runs far past their box, which draw the
/// part of it the box shows, or refuse it when it is centred so far off that a
/// viewer's rounding moves it. Twice as many cost what the added ones draw, and
/// for a name twice as long no more than a step more of a search through it.
#[test]
fn stamps_sharing_a_long_name_read_it_once() {
    const N: usize = 40;
    let names = [|n: usize| "#20".repeat(n), |n: usize| "ABCD".repeat(n / 4)];
    for name in names {
        for rect in ["[10 10 10 10]", "[10 10 200 60]"] {
            let annot = |_: usize| format!("/Subtype /Stamp /Name 4 0 R /Rect {rect}");
            let added = |n: usize| {
                let shared = [format!("/{}", name(n))];
                // A name is centred by its advance: a long one, refused.
                let (one, two) = (
                    cost_refused(N, "", &shared, &annot, true),
                    cost_refused(2 * N, "", &shared, &annot, true),
                );
                Cost {
                    bytes: two.bytes - one.bytes,
                    work: two.work - one.work,
                }
            };
            let (short, long) = (added(20_000), added(40_000));
            assert!(
                long.bytes < N * (64 << 10)
                    && long.work < (N * 4_000) as u64
                    && long.bytes < short.bytes + N * 256
                    && long.work < short.work + (N * 8) as u64,
                "{} {rect}: {short:?} added at 20,000, {long:?} at 40,000",
                &name(4)
            );
        }
    }
}

/// A glyph far along its line, past text the box does not show, is placed where it
/// lies, by numbers as small as its place: the end of a right-aligned word of 1,000
/// letters, the middle of a stamp name as long, is drawn, and only it. Of 40,000
/// letters, the word starts so far off that a viewer adding up its advances in
/// single precision may put its end anywhere within some points: it is refused.
#[test]
fn a_glyph_far_along_its_line_is_drawn() {
    for (letters, drawn_where) in [(1_000, true), (40_000, false)] {
        let word = "A".repeat(letters);
        for annot in [
            format!(
                "/Subtype /FreeText /Q 2 /DA (/Helv 12 Tf) /Contents ({word}) \
                 /Rect [10 10 190 190]"
            ),
            format!("/Subtype /Stamp /Name /{word} /Rect [10 10 200 60]"),
        ] {
            let pdf = one_annotation(&annot);
            match first_look(&pdf) {
                Look::Generated(g) if drawn_where => {
                    let drawn = g.content.matches(" Do").count();
                    assert!(
                        (1..100).contains(&drawn),
                        "{drawn} glyphs: {}",
                        &annot[..20]
                    );
                }
                Look::Unsupported(Unsupported::Coordinates) if !drawn_where => {}
                _ => panic!("{letters} letters: {}", &annot[..20]),
            }
        }
    }
}

/// Annotations that share a text whose lines each hold one token wider than any
/// box, and whose paragraphs are long runs of spaces, lay out only the lines they
/// show, each measured as viewers measure it up to the glyph that passes the room:
/// a token is passed whole by a search, and a space after a space is a token of
/// its own, each looked at once. `FreeText` contents and a multiline field's
/// value, each in its own width and size.
#[test]
fn lines_of_huge_tokens_and_runs_of_spaces_cost_what_they_show() {
    let huge = "W".repeat(2_000);
    let text = format!(
        "({} {}\\n{}\\n{} {})",
        huge,
        huge,
        " ".repeat(6_000),
        huge,
        " ".repeat(6_000)
    );
    let shared = vec![text];
    let thousandths = |i: usize| format!("{}.{:03}", 100 + i / 1000, i % 1000);
    shared_once("", &shared, |i| {
        format!(
            "/Subtype /FreeText /BS << /W 0 >> /Contents 4 0 R /Rect [10 10 {} 40] \
             /DA (/Helv {} Tf 0 g)",
            thousandths(i),
            thousandths(i).replace("100.", "10.")
        )
    });
    shared_once("", &shared, |i| {
        format!(
            "/Subtype /Widget /FT /Tx /Ff 4096 /V 4 0 R /Rect [10 10 {} 40] \
             /DA (/Helv {} Tf 0 g)",
            thousandths(i),
            thousandths(i).replace("100.", "10.")
        )
    });
    // Starting below a run of spaces as tall as the box shows: the box shows
    // the spaces' lines alone, each of a few dozen tokens.
    let spaces = vec![format!("({})", " ".repeat(20_000))];
    shared_once("", &spaces, |i| {
        format!(
            "/Subtype /Widget /FT /Tx /Ff 4096 /V 4 0 R /Rect [10 10 {} 40] \
             /DA (/Helv {} Tf 0 g)",
            thousandths(i),
            thousandths(i).replace("100.", "10.")
        )
    });
}
