//! A generated appearance costs what its visible result needs, however large the
//! numbers in the annotation, and what the file holds, however its objects refer to
//! each other. These tests run under an allocation limit: an appearance whose cost
//! grows with those numbers makes the allocator refuse, and the test binary aborts
//! ("memory allocation of … bytes failed") instead of exhausting the machine's
//! memory. Work that allocates as it goes is measured by the bytes each thread has
//! allocated in all ([`allocated`]).

use hayro_interpret::InterpreterSettings;
use hayro_interpret::annotation::{Appearances, Look, Unsupported, work_done};
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
    // Cells 500 pt wide: the page shows the divider at 2 pt (501 in the field's frame,
    // which starts at -499); the one at 502 pt is drawn too, the search for those
    // shown keeping a step of margin.
    let content = generated(
        "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /BC [0] >> /MaxLen 16000 \
         /Rect [-499 100 8000000 130]",
    );
    let starts: Vec<i64> = content
        .lines()
        .filter(|l| l.ends_with(" m"))
        .map(|l| l.split(' ').next().unwrap().parse::<f64>().unwrap().round() as i64)
        .collect();
    assert_eq!(starts, [501, 1001], "{content}");
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
    // Steps of 0.0101 pt across a quad 4·10^6 pt wide: about 20 000 on the page.
    let content = generated(
        "/Subtype /Squiggly /C [0] /QuadPoints [0 100.0707 4000000 100.0707 0 100 4000000 100] \
         /Rect [0 100 200 101]",
    );
    let steps = content.matches(" l\n").count();
    assert!((19_000..21_000).contains(&steps), "{steps} steps");
}

/// 4·10^6: twice it is below 2^23, from which numbers are not written.
fn huge() -> String {
    "4000000".to_owned()
}

/// A radio button as large as a written number allows is four curves for each circle,
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

/// So is a circle line ending of a line as wide as its ending allows.
#[test]
fn a_huge_circle_ending_costs_what_a_small_one_does() {
    let h = "1000000";
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
        interpret_page(page, &mut ctx, &mut DummyDevice);
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
    let annots: Vec<Dict<'_>> = pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .collect();
    let before = allocated();
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    for annot in &annots {
        let Look::Generated(a) = appearances.look(annot, PAGE) else {
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
    let annots: Vec<Dict<'_>> = pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .collect();
    let before = allocated();
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    for a in &annots {
        let Look::Generated(a) = appearances.look(a, PAGE) else {
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
        let annot = pdf.pages()[0]
            .raw()
            .get::<Array<'_>>(b"Annots")
            .unwrap()
            .iter::<Dict<'_>>()
            .next()
            .unwrap();
        let appearances =
            Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
        let before = allocated();
        let _ = hayro_interpret::annotation::flattened(&annot);
        let _ = hayro_interpret::annotation::flags(&annot);
        assert!(
            matches!(appearances.look(&annot, PAGE), Look::Generated(_)),
            "{annot:?}"
        );
        let bytes = allocated() - before;
        assert!(bytes < 1 << 20, "{bytes} bytes: {annot:?}");
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
    let annots: Vec<Dict<'_>> = pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .collect();
    let (bytes, work) = (allocated(), work_done());
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    for a in &annots {
        match appearances.look(a, PAGE) {
            Look::Generated(g) => assert!(g.content.len() < 64 << 10, "{}", annot(0)),
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
    const N: usize = 40;
    let (one, two) = (
        cost_of(N, catalog, shared, &annot),
        cost_of(2 * N, catalog, shared, &annot),
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
    shared_once("", &text, |i| {
        format!(
            "/Subtype /Widget /FT /Btn /Ff 65536 /MK << /CA 4 0 R >> /Rect [10 10 {} 40]",
            thousandths(i)
        )
    });
    shared_once("", &text, |i| {
        format!(
            "/Subtype /Line /C [0] /Cap true /Contents 4 0 R /L [20 100 {} 100] /Rect [0 0 200 200]",
            thousandths(i)
        )
    });
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
            // Some 100 bytes an option, kept for the document, and work for the
            // dozen bytes it is written in; setting each option would take some
            // 20,000 characters' worth.
            assert!(
                two.bytes - one.bytes < 1000 * 512 && two.work - one.work < 1000 * 64,
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
    let pdf = Pdf::new(common::one_page(
        200,
        "/Annots [4 0 R]",
        vec![format!("<< /Type /Annot {mixed} >>").into_bytes()],
    ))
    .expect("valid pdf");
    let annot = pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .next()
        .unwrap();
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    assert!(matches!(
        appearances.look(&annot, PAGE),
        Look::Unsupported(Unsupported::RichText)
    ));
}

/// A glyph is written once for an appearance, as a form, and each place it is drawn
/// costs a few bytes: a comb of 10,000 cells, all shown, and a `FreeText` full of
/// text, each some 30 bytes a glyph drawn, as `bake` writes some 11 (before either
/// is compressed).
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
        let pdf = Pdf::new(common::one_page(
            200,
            "/Annots [4 0 R]",
            vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
        ))
        .expect("valid pdf");
        let a = pdf.pages()[0]
            .raw()
            .get::<Array<'_>>(b"Annots")
            .unwrap()
            .iter::<Dict<'_>>()
            .next()
            .unwrap();
        let appearances =
            Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
        let Look::Generated(g) = appearances.look(&a, PAGE) else {
            panic!("{annot}: not generated")
        };
        let drawn = g.content.matches(" Do\n").count();
        assert!(drawn > 1_000, "{drawn}");
        assert!(g.glyphs.len() <= 20, "{}", g.glyphs.len());
        let per_glyph = g.content.len() / drawn;
        assert!(per_glyph <= 32, "{per_glyph} bytes a glyph");
    }
}
