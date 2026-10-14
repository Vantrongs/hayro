//! A generated appearance costs what its visible result needs, however large the
//! numbers in the annotation, and what the file holds, however its objects refer to
//! each other. These tests run under an allocation limit: an appearance whose cost
//! grows with those numbers makes the allocator refuse, and the test binary aborts
//! ("memory allocation of … bytes failed") instead of exhausting the machine's
//! memory. Work that allocates as it goes is measured by the bytes each thread has
//! allocated in all ([`allocated`]).

use hayro_interpret::InterpreterSettings;
use hayro_interpret::annotation::{Appearances, Look, Unsupported};
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::object::{Array, Dict};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;

/// The system allocator, refusing to hold more than [`LIMIT`] bytes at once, and
/// counting the bytes each thread allocates.
struct Limited;

const LIMIT: usize = 256 << 20;
static HELD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
}

/// The bytes this thread has allocated so far, freed or not.
fn allocated() -> usize {
    ALLOCATED.with(Cell::get)
}

// SAFETY: forwards to the system allocator, refusing some requests with null as
// `GlobalAlloc::alloc` allows.
unsafe impl GlobalAlloc for Limited {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if HELD.fetch_add(layout.size(), Ordering::Relaxed) + layout.size() > LIMIT {
            HELD.fetch_sub(layout.size(), Ordering::Relaxed);
            return std::ptr::null_mut();
        }
        // A thread being torn down has no counter left; its allocations count for none.
        let _ = ALLOCATED.try_with(|a| a.set(a.get() + layout.size()));
        // SAFETY: the caller's layout, as `alloc` requires.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: allocated by `alloc` above with this layout.
        unsafe { System.dealloc(ptr, layout) };
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Limited = Limited;

/// A page 200 pt square.
const PAGE: kurbo::Rect = kurbo::Rect::new(0.0, 0.0, 200.0, 200.0);

/// The generated content of the annotation `annot` on a 200 pt page.
fn generated(annot: &str) -> String {
    let pdf = Pdf::new(common::one_page(
        200,
        "/Annots [4 0 R]",
        vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
    ))
    .expect("valid pdf");
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let annots: Vec<Dict<'_>> = pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .collect();
    match appearances.look(&annots[0], PAGE) {
        Look::Generated(a) => a.content,
        _ => panic!("{annot}: not generated"),
    }
}

/// The number of lines drawn, as `m` operators.
fn lines(content: &str) -> usize {
    content.lines().filter(|l| l.ends_with(" m")).count()
}

/// Comb cells closer than can be told apart are drawn as the band their dividers
/// cover, at a cost that does not grow with `/MaxLen`.
#[test]
fn a_comb_of_billions_of_cells_is_drawn_as_its_band() {
    let comb = "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /BC [0] >>";
    for (max_len, w) in [("2147483647", "1"), ("100000000", "0.000001")] {
        let content = generated(&format!(
            "{comb} /MaxLen {max_len} /BS << /W {w} >> /Rect [20 100 180 130]"
        ));
        assert_eq!(lines(&content), 1, "{max_len}: {content}");
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
    // Cells 500 pt wide: the page shows the divider at 2 pt; the one at 502 pt is
    // drawn too, the search for those shown keeping a step of margin.
    let content = generated(
        "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /BC [0] >> /MaxLen 2000000000 \
         /Rect [-499 100 999999999999 130]",
    );
    let starts: Vec<i64> = content
        .lines()
        .filter(|l| l.ends_with(" m"))
        .map(|l| l.split(' ').next().unwrap().parse::<f64>().unwrap().round() as i64)
        .collect();
    assert_eq!(starts, [2, 502], "{content}");
}

/// A squiggle whose steps are closer than can be told apart is drawn as the band
/// it covers; one of steps just apart is drawn across the page only.
#[test]
fn a_flat_squiggle_is_drawn_at_a_bounded_cost() {
    let content = generated(
        "/Subtype /Squiggly /C [0] /QuadPoints [0 0.000000000001 200 0.000000000001 0 0 200 0] \
         /Rect [0 0 200 1]",
    );
    assert_eq!(lines(&content), 1, "{content}");
    assert!(content.contains(" l\nS\n"), "{content}");
    // Steps of 0.0101 pt across a quad 10^9 pt wide: about 20 000 on the page.
    let content = generated(
        "/Subtype /Squiggly /C [0] /QuadPoints [0 100.0707 1000000000 100.0707 0 100 1000000000 100] \
         /Rect [0 100 200 101]",
    );
    let steps = content.matches(" l\n").count();
    assert!((19_000..21_000).contains(&steps), "{steps} steps");
}

/// 10^38, near the largest 32-bit number.
fn huge() -> String {
    format!("1{}", "0".repeat(38))
}

/// A radio button as large as a 32-bit number allows is four curves for each circle,
/// as viewers draw it: the work does not grow with its size.
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

/// So is a circle line ending of a line that wide.
#[test]
fn a_huge_circle_ending_costs_what_a_small_one_does() {
    let h = huge();
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
    let annots: Vec<Dict<'_>> = pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .collect();
    let before = allocated();
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    for annot in &annots {
        assert!(matches!(appearances.look(annot, PAGE), Look::Generated(_)));
    }
    let bytes = allocated() - before;
    // About 4 KiB a widget; reading the whole chain for each takes gigabytes.
    assert!(bytes < 32 << 20, "{bytes} bytes");
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
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let annot = pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .next()
        .unwrap();
    assert!(matches!(
        appearances.look(&annot, PAGE),
        Look::Unsupported(Unsupported::Coordinates)
    ));
}

/// What `annot` looks like on a 200 pt page: its generated content, or why it has
/// none. Fails the test unless it is done within `secs` seconds.
fn look_within(secs: u64, annot: String) -> Result<String, Unsupported> {
    let (sent, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pdf = Pdf::new(common::one_page(
            200,
            "/Annots [4 0 R]",
            vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
        ))
        .expect("valid pdf");
        let appearances =
            Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
        let annots: Vec<Dict<'_>> = pdf.pages()[0]
            .raw()
            .get::<Array<'_>>(b"Annots")
            .unwrap()
            .iter::<Dict<'_>>()
            .collect();
        let look = match appearances.look(&annots[0], PAGE) {
            Look::Generated(a) => Ok(a.content),
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
