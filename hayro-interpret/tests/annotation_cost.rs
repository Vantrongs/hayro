//! A generated appearance costs what its visible result needs, however large the
//! numbers in the annotation, and what the file holds, however its objects refer to
//! each other. These tests run under an allocation limit: an appearance whose cost
//! grows with those numbers makes the allocator refuse, and the test binary aborts
//! ("memory allocation of … bytes failed") instead of exhausting the machine's
//! memory. Work that allocates as it goes is measured by the bytes each thread has
//! allocated in all ([`allocated`]).

use hayro_interpret::annotation::{Appearances, Drawn, Look, Unsupported, work_done};
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::object::ObjectIdentifier;
use hayro_interpret::hayro_syntax::page::Page;
use hayro_interpret::hayro_syntax::work;
use hayro_interpret::{AnnotationSelection, InterpreterSettings};
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

/// The annotations the first page of `pdf` draws as viewed on a 200 pt page.
fn drawn<'a>(appearances: &Appearances<'a>, pdf: &'a Pdf) -> std::rc::Rc<[Drawn<'a>]> {
    appearances.page(&pdf.pages()[0], AnnotationSelection::View, PAGE)
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

/// Comb dividers that touch are drawn as the band they cover, at a cost that does
/// not grow with `/MaxLen`; dividers apart are drawn each, however close, those the
/// page shows, and a comb that would draw more than 262 144 is refused.
#[test]
fn comb_dividers_are_drawn_each_unless_they_touch() {
    let comb = |max_len: &str, w: &str, rect: &str| {
        let pdf = one_annotation(&format!(
            "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /BC [0] >> /MaxLen {max_len} \
             /BS << /W {w} >> /Rect {rect}"
        ));
        match first_look(&pdf) {
            Look::Generated(a) => Ok(lines(&a.content)),
            Look::Unsupported(why) => Err(why),
            _ => panic!("{max_len} {w}: drawn otherwise"),
        }
    };
    let field = "[20 100 180 130]";
    // The band (the border is a rectangle).
    assert_eq!(comb("2147483647", "1", field), Ok(1));
    // Dividers 0.001 pt wide, 0.005 pt apart, each shown: as many as /MaxLen less one.
    assert_eq!(comb("32000", "0.001", field), Ok(31_999));
    // Of a field 160 times as wide as the page, with dividers about 1 pt apart, the
    // 199 on the page and the next.
    assert_eq!(comb("32000", "0.001", "[0 100 32000 130]"), Ok(200));
    assert_eq!(
        comb("100000000", "0.000001", field),
        Err(Unsupported::Coordinates)
    );
    // A field above the page shows none of its dividers, however many.
    assert_eq!(comb("1000000", "0.000001", "[20 1000 180 1030]"), Ok(0));
    // Nor does one beside the page, turned so that the rectangle holding the page,
    // in the field's space, reaches the field.
    let turned = one_annotation(
        "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /R 45 /BC [0] >> /MaxLen 1000000 \
         /BS << /W .000001 >> /Rect [201 20 361 50]",
    );
    match first_look(&turned) {
        Look::Generated(a) => assert_eq!(lines(&a.content), 0),
        _ => panic!("turned comb beside the page: drawn otherwise"),
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

/// A squiggle is drawn step by step, however fine, across the page only; one that
/// would draw more than 262 144 steps there is refused.
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
    // Steps of 0.001 pt across a quad 4·10^6 pt wide: about 200 000 on the page.
    let n = steps("0 100.007 4000000 100.007 0 100 4000000 100").unwrap();
    assert!((199_000..201_000).contains(&n), "{n} steps");
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
        let flattened = appearances.page(page, AnnotationSelection::Flatten, PAGE);
        let viewed = appearances.page(page, AnnotationSelection::View, PAGE);
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
        let pdf = one_annotation(&annot);
        let Look::Generated(g) = first_look(&pdf) else {
            panic!("{annot}: not generated")
        };
        let drawn = g.content.matches(" Do\n").count();
        assert!(drawn > 1_000, "{drawn}");
        assert!(g.glyphs.len() <= 20, "{}", g.glyphs.len());
        let per_glyph = g.content.len() / drawn;
        assert!(per_glyph <= 32, "{per_glyph} bytes a glyph");
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
}

/// The bytes the readers of this thread have looked at so far
/// ([`hayro_interpret::hayro_syntax::work::EXAMINED`]).
fn examined() -> usize {
    work::EXAMINED.get()
}

/// The cost of `f`, given the appearances of `pdf`'s annotations, made inside.
fn listed<'a>(pdf: &'a Pdf, f: impl FnOnce(&Appearances<'a>) -> Drew) -> Listed {
    let (bytes, work, looked, start) = (
        allocated(),
        work_done(),
        examined(),
        std::time::Instant::now(),
    );
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let drew = f(&appearances);
    Listed {
        bytes: allocated() - bytes,
        work: work_done() - work,
        examined: examined() - looked,
        secs: start.elapsed().as_secs_f64(),
        drew,
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
        .page(page, AnnotationSelection::View, visible)
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
        costs_at_most(fewer, more, 16 << 10, 1_000, annot);
    }
}

/// Stamps that share a long name read it once for the document, and set it once:
/// those of no area, which draw nothing; those whose name is spaces, which draw
/// their frame alone; and those whose name runs far past their box, which draw the
/// part of it the box shows. Twice as many cost what the added ones draw, and no
/// more for a name twice as long.
#[test]
fn stamps_sharing_a_long_name_read_it_once() {
    const N: usize = 40;
    let names = [|n: usize| "#20".repeat(n), |n: usize| "ABCD".repeat(n / 4)];
    for name in names {
        for rect in ["[10 10 10 10]", "[10 10 200 60]"] {
            let annot = |_: usize| format!("/Subtype /Stamp /Name 4 0 R /Rect {rect}");
            let added = |n: usize| {
                let shared = [format!("/{}", name(n))];
                let (one, two) = (
                    cost_of(N, "", &shared, &annot),
                    cost_of(2 * N, "", &shared, &annot),
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
                    && long.work < short.work + (N * 4) as u64,
                "{} {rect}: {short:?} added at 20,000, {long:?} at 40,000",
                &name(4)
            );
        }
    }
}

/// A glyph far along its line, past text the box does not show, is placed where it
/// lies, by numbers as small as its place: the end of a long right-aligned word,
/// the middle of a long stamp name, is drawn, and only it.
#[test]
fn a_glyph_far_along_its_line_is_drawn() {
    let word = "A".repeat(40_000);
    for annot in [
        format!(
            "/Subtype /FreeText /Q 2 /DA (/Helv 12 Tf) /Contents ({word}) /Rect [10 10 190 190]"
        ),
        format!("/Subtype /Stamp /Name /{word} /Rect [10 10 200 60]"),
    ] {
        let pdf = one_annotation(&annot);
        let Look::Generated(g) = first_look(&pdf) else {
            panic!("not generated: {}", &annot[..20]);
        };
        let drawn = g.content.matches(" Do\n").count();
        assert!(
            (1..100).contains(&drawn),
            "{drawn} glyphs: {}",
            &annot[..20]
        );
    }
}
