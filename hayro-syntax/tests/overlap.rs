//! Damaged files whose objects overlap: places an entry leads to inside another
//! object's body, in the file or in an object stream, and object streams whose data
//! reach over one another. Each body is lexed in its slot, up to the next place, and
//! an object stream's data end at the next place holding a stream, so that reading
//! every object takes work and memory linear in the file; and a composite value is
//! parsed once, however many typed reads of it are made.

use hayro_syntax::Pdf;
use hayro_syntax::object::{Array, Dict, Name, Number, ObjectIdentifier, Rect};
use hayro_syntax::work::{BODIES_READ, LEXED, OBJECT_STREAM_ENTRIES};

mod common;
use common::allocations::measure;

const CATALOG: &[u8] = b"<< /Type /Catalog /Pages 2 0 R >>";

/// A file of `objects`, each at its number, then `extra`, with a table that puts
/// each of `inside` at its offset in `extra`; where `broken`, `startxref` leads to
/// no table, which rebuilds it.
fn table_file(
    objects: &[(u32, Vec<u8>)],
    extra: &[u8],
    inside: &[(u32, usize)],
    broken: bool,
) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut entries = std::collections::BTreeMap::new();
    for (number, body) in objects {
        entries.insert(*number, out.len());
        out.extend(format!("{number} 0 obj\n").bytes());
        out.extend(body);
        out.extend(b"\nendobj\n");
    }
    let base = out.len();
    out.extend(extra);
    out.push(b'\n');
    for (number, at) in inside {
        entries.insert(*number, base + at);
    }
    let size = entries.keys().max().unwrap() + 1;
    let table = out.len();
    out.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").bytes());
    for number in 1..size {
        match entries.get(&number) {
            Some(at) => out.extend(format!("{at:010} 00000 n \n").bytes()),
            None => out.extend(b"0000000000 65535 f \n"),
        }
    }
    let start = if broken { 9 } else { table };
    out.extend(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n").bytes(),
    );
    out
}

/// A file of `objects`, each at its number, with a cross-reference stream that puts
/// each of `compressed`, (number, stream, index), in an object stream; where
/// `broken`, `startxref` leads to no table, which rebuilds it.
fn stream_table(
    objects: &[(u32, Vec<u8>)],
    compressed: &[(u32, u32, u32)],
    broken: bool,
) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut entries = std::collections::BTreeMap::new();
    for (number, body) in objects {
        entries.insert(*number, (1_u8, out.len() as u32, 0_u16));
        out.extend(format!("{number} 0 obj\n").bytes());
        out.extend(body);
        out.extend(b"\nendobj\n");
    }
    for &(number, stream, index) in compressed {
        entries.insert(number, (2, stream, index as u16));
    }
    let table = entries.keys().max().unwrap() + 1;
    let at = out.len();
    entries.insert(table, (1, at as u32, 0));
    let mut rows = vec![];
    for number in 0..=table {
        let (kind, field, index) = entries.get(&number).copied().unwrap_or((0, 0, 0));
        rows.push(kind);
        rows.extend(field.to_be_bytes());
        rows.extend(index.to_be_bytes());
    }
    out.extend(
        format!(
            "{table} 0 obj\n<< /Type /XRef /Size {} /W [1 4 2] /Root 1 0 R /Length {} >>\nstream\n",
            table + 1,
            rows.len()
        )
        .bytes(),
    );
    out.extend(rows);
    let start = if broken { 9 } else { at };
    out.extend(format!("\nendstream\nendobj\nstartxref\n{start}\n%%EOF\n").bytes());
    out
}

/// The catalog, the page tree and `n` pages, objects 3 to `n` + 2, page `p` with
/// `/UserUnit` object `n` + 3 + `p`; and that first number.
fn pages(n: u32) -> (Vec<(u32, Vec<u8>)>, u32) {
    let first = n + 3;
    let kids: Vec<String> = (0..n).map(|p| format!("{} 0 R", p + 3)).collect();
    let mut objects = vec![
        (1, CATALOG.to_vec()),
        (
            2,
            format!(
                "<< /Type /Pages /Kids [{}] /Count {n} /MediaBox [0 0 600 700] >>",
                kids.join(" ")
            )
            .into_bytes(),
        ),
    ];
    for p in 0..n {
        objects.push((
            p + 3,
            format!(
                "<< /Type /Page /Parent 2 0 R /UserUnit {} 0 R >>",
                first + p
            )
            .into_bytes(),
        ));
    }
    (objects, first)
}

/// `n` objects whose headers, `k 0 obj (`, are nested, each in the string of the
/// one before, with one tail of `512 n` escapes and `n` closings after the last.
fn nested_in_the_file(n: u32, broken: bool) -> Vec<u8> {
    let (objects, first) = pages(n);
    let mut body = vec![];
    let mut inside = vec![];
    for k in first..first + n {
        inside.push((k, body.len()));
        body.extend(format!("{k} 0 obj (").bytes());
    }
    body.extend(b"\\101".repeat(512 * n as usize));
    body.extend(b") endobj".repeat(n as usize));
    table_file(&objects, &body, &inside, broken)
}

/// An object stream whose header puts its `n` objects at offsets 0 to `n` - 1, into
/// `n` opening parentheses, a tail of `512 n` escapes, and `n` closing ones.
fn nested_in_an_object_stream(n: u32) -> Vec<u8> {
    let (mut objects, first) = pages(n);
    let mut payload = b"(".repeat(n as usize);
    payload.extend(b"\\101".repeat(512 * n as usize));
    payload.extend(b")".repeat(n as usize));
    let header: String = (0..n).map(|i| format!("{} {i} ", first + i)).collect();
    let stream = first + n;
    let mut body = format!(
        "<< /Type /ObjStm /N {n} /First {} /Length {} >>\nstream\n{header}",
        header.len(),
        header.len() + payload.len()
    )
    .into_bytes();
    body.extend(payload);
    body.extend(b"\nendstream");
    objects.push((stream, body));
    let compressed: Vec<_> = (0..n).map(|i| (first + i, stream, i)).collect();
    stream_table(&objects, &compressed, false)
}

/// How the /Length of each of [`overlapping_object_streams`] leads to `endstream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Length {
    /// Past the pad, to it.
    Whole,
    /// To the pad, which is white space before it.
    Padded,
    /// Nowhere: it is searched for.
    Wrong,
}

/// `n` object streams, one after another, the data of each reaching over those after
/// it to one pad of `512 n` spaces and the one `endstream`, as `length` leads there;
/// object `first` + `i` is the first of stream `i`, 5.
fn overlapping_object_streams(n: u32, length: Length, broken: bool) -> Vec<u8> {
    let (mut objects, first) = pages(n);
    let mut out = vec![];
    let mut lengths = vec![];
    for i in 0..n {
        let header = format!("{} 0 ", first + i);
        out.extend(
            format!(
                "{} 0 obj\n<< /Type /ObjStm /N 1 /First {} /Length LLLLLLLLLL >>\nstream\n",
                first + n + i,
                header.len()
            )
            .bytes(),
        );
        lengths.push(out.len());
        out.extend(header.bytes());
        out.extend(b"5 ");
    }
    let pad = out.len();
    out.extend(b" ".repeat(512 * n as usize));
    let end = out.len();
    out.extend(b"\nendstream\nendobj\n");
    for (i, data) in lengths.iter().enumerate() {
        let length = match length {
            Length::Whole => end - data,
            Length::Padded => pad - data,
            Length::Wrong => 0,
        };
        let at = data - b" >>\nstream\n".len() - 10;
        out[at..at + 10].copy_from_slice(format!("{length:10}").as_bytes());
        assert_eq!(&out[at - 8..at], b"/Length ", "stream {i}");
    }
    // The streams are written as one object's bytes, and the table puts each at its
    // header.
    let mut file = b"%PDF-1.7\n".to_vec();
    let mut entries = std::collections::BTreeMap::new();
    for (number, body) in objects.drain(..) {
        entries.insert(number, (1_u8, file.len() as u32, 0_u16));
        file.extend(format!("{number} 0 obj\n").bytes());
        file.extend(body);
        file.extend(b"\nendobj\n");
    }
    let base = file.len();
    let mut at = 0;
    for i in 0..n {
        let header = format!("{} 0 obj", first + n + i);
        let found = out[at..]
            .windows(header.len())
            .position(|w| w == header.as_bytes())
            .unwrap();
        entries.insert(first + n + i, (1, (base + at + found) as u32, 0));
        entries.insert(first + i, (2, first + n + i, 0));
        at += found + header.len();
    }
    file.extend(&out);
    let table = entries.keys().max().unwrap() + 1;
    let offset = file.len();
    entries.insert(table, (1, offset as u32, 0));
    let mut rows = vec![];
    for number in 0..=table {
        let (kind, field, index) = entries.get(&number).copied().unwrap_or((0, 0, 0));
        rows.push(kind);
        rows.extend(field.to_be_bytes());
        rows.extend(index.to_be_bytes());
    }
    file.extend(
        format!(
            "{table} 0 obj\n<< /Type /XRef /Size {} /W [1 4 2] /Root 1 0 R /Length {} >>\nstream\n",
            table + 1,
            rows.len()
        )
        .bytes(),
    );
    file.extend(rows);
    let start = if broken { 9 } else { offset };
    file.extend(format!("\nendstream\nendobj\nstartxref\n{start}\n%%EOF\n").bytes());
    file
}

/// What loading `file` and reading each page's media box and `/UserUnit` finds: the
/// pages, each page's unit, and what reading it lost; and the bytes lexed and the
/// most bytes held at once doing so.
#[allow(clippy::type_complexity)]
fn loaded(
    file: Vec<u8>,
) -> (
    (usize, Vec<(Option<f32>, Option<ObjectIdentifier>)>),
    (usize, usize),
) {
    BODIES_READ.set(0);
    LEXED.set(0);
    let (found, used) = measure(move || {
        let pdf = Pdf::new(file).unwrap();
        let pages = pdf.pages();
        let units = pages
            .iter()
            .map(|page| {
                assert_eq!((page.media_box().x1, page.media_box().y1), (600.0, 700.0));
                pdf.xref().watch(|| page.raw().get::<f32>(b"UserUnit"))
            })
            .collect();
        (pages.len(), units)
    });
    (found, (BODIES_READ.get() + LEXED.get(), used.peak))
}

/// Asserts that `cost` of 2 `n` is at most about twice that of `n`.
fn linear(family: &str, cost: impl Fn(u32) -> (usize, usize)) {
    let [small, large] = [128, 256].map(&cost);
    assert!(
        large.0 <= small.0 * 21 / 10 + 4096 && large.1 <= small.1 * 21 / 10 + 4096,
        "{family}: (bytes lexed, peak bytes held) {small:?} for 128, {large:?} for 256"
    );
}

// Entries that lead into the string of the object before, all sharing one tail: each
// body is lexed in its slot, where no string ends, so each but the innermost is lost,
// and is read once, as is the tail; the pages keep their geometry.
#[test]
fn objects_nested_in_the_file_are_read_in_linear_work_and_memory() {
    for broken in [false, true] {
        linear("nested in the file", |n| {
            loaded(nested_in_the_file(n, broken)).1
        });
        let n = 64;
        let ((pages, units), _) = loaded(nested_in_the_file(n, broken));
        assert_eq!(pages, n as usize);
        for (p, (unit, lost)) in units.into_iter().enumerate() {
            let id = ObjectIdentifier::new((n + 3) as i32 + p as i32, 0);
            // The innermost is a string, read, but no number. A rebuilt table finds
            // the outermost only, whose string holds the others.
            let expected = if broken || p == n as usize - 1 {
                None
            } else {
                Some(id)
            };
            assert_eq!((unit, lost), (None, expected), "page {p}, broken {broken}");
        }
    }
}

// Objects of an object stream at offsets inside one another: each is lexed in its
// slot, up to the next offset, so each but the last is lost, and the tail is read
// once.
#[test]
fn objects_nested_in_an_object_stream_are_read_in_linear_work_and_memory() {
    linear("nested in an object stream", |n| {
        loaded(nested_in_an_object_stream(n)).1
    });
    let n = 64;
    let ((pages, units), _) = loaded(nested_in_an_object_stream(n));
    assert_eq!(pages, n as usize);
    for (p, (unit, lost)) in units.into_iter().enumerate() {
        let id = ObjectIdentifier::new((n + 3) as i32 + p as i32, 0);
        let expected = if p == n as usize - 1 { None } else { Some(id) };
        assert_eq!((unit, lost), (None, expected), "page {p}");
    }
}

// Object streams whose data, by /Length or by the one `endstream`, reach over the
// streams after them: each is cut at the next, so that what is decoded and kept is
// linear in the file, and each still holds its object, read and kept once; finding
// the data, and the `endstream` after white space, searches the file once, whatever
// table finds the streams. A rebuilt table finds only the first stream where its
// /Length leads to the `endstream`: the others' headers are in its data, and bytes in
// a stream's data are its content, not the file's objects, or an attached PDF's
// objects would replace the document's (`a_rebuild_takes_no_object_of_an_embedded_pdf`
// in `xref.rs`); PyMuPDF 1.28.2 also finds 1 of the 64 values. Where /Length leads
// nowhere, nothing shows the data reach the `endstream`, and the other streams start
// before it, as where a stream's own `endstream` is missing: a rebuilt table finds
// every stream, as the rebuild before 9b38fdbe did, where PyMuPDF finds the first.
#[test]
fn overlapping_object_streams_are_read_in_linear_work_and_memory() {
    for length in [Length::Whole, Length::Padded, Length::Wrong] {
        for broken in [false, true] {
            let ((pages, units), _) = loaded(overlapping_object_streams(64, length, broken));
            assert_eq!(pages, 64);
            assert!(
                units.iter().enumerate().all(|(page, unit)| *unit
                    == if broken && page > 0 && length != Length::Wrong {
                        (None, None)
                    } else {
                        (Some(5.0), None)
                    }),
                "{length:?}, broken {broken}: {units:?}"
            );
            linear(&format!("overlapping, {length:?}, broken {broken}"), |n| {
                loaded(overlapping_object_streams(n, length, broken)).1
            });
        }
    }
}

/// A page with `n` annotations that share one indirect /Rect, `[0`, `pad` spaces,
/// `0 50 50]`.
fn shared_rect(n: u32, pad: usize) -> Vec<u8> {
    let rect = n + 4;
    let annots: Vec<String> = (0..n).map(|i| format!("{} 0 R", rect + 1 + i)).collect();
    let mut objects = vec![
        (1, CATALOG.to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Annots [{}] >>",
                annots.join(" ")
            )
            .into_bytes(),
        ),
        (rect, format!("[0 {}0 50 50]", " ".repeat(pad)).into_bytes()),
    ];
    for i in 0..n {
        objects.push((
            rect + 1 + i,
            format!("<< /Type /Annot /Subtype /Square /Rect {rect} 0 R /C [1 0 0] >>").into_bytes(),
        ));
    }
    table_file(&objects, b"", &[], false)
}

// Annotations that share one padded rectangle read it as a `Rect` each: the array is
// parsed once, and no read lexes it again.
#[test]
fn a_rectangle_shared_by_annotations_is_parsed_once() {
    let cost = |n: u32| {
        let pdf = Pdf::new(shared_rect(n, 512 * n as usize)).unwrap();
        let page = &pdf.pages()[0];
        BODIES_READ.set(0);
        LEXED.set(0);
        let annots = page.raw().get::<Array<'_>>(b"Annots").unwrap();
        for annot in annots.iter::<Dict<'_>>() {
            let rect = annot.get::<Rect>(b"Rect").unwrap();
            assert_eq!((rect.x0, rect.x1, rect.y1), (0.0, 50.0, 50.0));
            assert_eq!(annot.get::<[f32; 3]>(b"C"), Some([1.0, 0.0, 0.0]));
        }
        assert_eq!(LEXED.get(), 0, "{n} annotations");
        (BODIES_READ.get(), 0)
    };
    linear("shared rectangle", cost);
}

/// A small deterministic generator of the property test's values.
struct Seed(u64);

impl Seed {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % below
    }

    fn space(&mut self) -> String {
        [" ", "  ", "\n", " % note\n", "\r\n "][self.next(5) as usize].to_owned()
    }
}

// Every typed read of a composite value, an array or a dictionary, direct, indirect,
// in an array or a dictionary, reads what the body it is in was parsed into: none
// lexes its bytes, and the body is read once, whatever the reads and however they are
// spaced.
#[test]
fn typed_reads_of_composite_values_lex_nothing() {
    let mut seed = Seed(7);
    for case in 0..64 {
        let numbers: Vec<String> = (0..4)
            .map(|_| (seed.next(200) as i64 - 100).to_string())
            .collect();
        let mut s = || seed.space();
        let array = format!(
            "[{}{}{}{}{}{}{}{}]",
            s(),
            numbers[0],
            s(),
            numbers[1],
            s(),
            numbers[2],
            s(),
            numbers[3]
        );
        let tuple = format!("[{}{}/N{}({}){}]", s(), numbers[0], s(), numbers[1], s());
        let holder = format!(
            "<< /R {array}{}/I 5 0 R{}/T {tuple}{}/D << /R {array} /A [{array} 5 0 R] >> /V [{array}{}{array}] >>",
            s(),
            s(),
            s(),
            s()
        );
        let objects = vec![
            (1, CATALOG.to_vec()),
            (2, b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec()),
            (4, holder.clone().into_bytes()),
            (5, array.clone().into_bytes()),
        ];
        let file = table_file(&objects, b"", &[], false);
        let pdf = Pdf::new(file.clone()).unwrap();
        let numbers: Vec<f32> = numbers.iter().map(|n| n.parse().unwrap()).collect();
        let rect = |r: Rect| [r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32];
        let sorted = {
            let (x0, x1) = (numbers[0].min(numbers[2]), numbers[0].max(numbers[2]));
            let (y0, y1) = (numbers[1].min(numbers[3]), numbers[1].max(numbers[3]));
            [x0, y0, x1, y1]
        };
        BODIES_READ.set(0);
        LEXED.set(0);
        for _ in 0..3 {
            let dict = pdf
                .xref()
                .get::<Dict<'_>>(ObjectIdentifier::new(4, 0))
                .unwrap();
            assert_eq!(
                dict.get::<Rect>(b"R").map(rect),
                Some(sorted),
                "case {case}"
            );
            assert_eq!(
                dict.get::<Rect>(b"I").map(rect),
                Some(sorted),
                "case {case}"
            );
            assert_eq!(
                dict.get::<[f32; 4]>(b"I").map(|a| a.to_vec()),
                Some(numbers.clone())
            );
            let all: Vec<f32> = dict
                .get::<Vec<Number>>(b"R")
                .unwrap()
                .iter()
                .map(|n| n.as_f64() as f32)
                .collect();
            assert_eq!(all, numbers, "case {case}");
            let t = dict.get::<Array<'_>>(b"T").unwrap();
            let (first, _, _) = t
                .flex_iter()
                .next::<(i32, Name<'_>, hayro_syntax::object::String<'_>)>()
                .unwrap();
            assert_eq!(first as f32, numbers[0], "case {case}");
            let mut flex = t.flex_iter();
            assert_eq!(flex.next::<i32>().map(|n| n as f32), Some(numbers[0]));
            assert!(flex.next::<i32>().is_none());
            assert!(flex.next::<Name<'_>>().is_some());
            let nested = dict.get::<Dict<'_>>(b"D").unwrap();
            assert_eq!(
                nested.get::<Rect>(b"R").map(rect),
                Some(sorted),
                "case {case}"
            );
            let pair = nested.get::<Array<'_>>(b"A").unwrap();
            let rects: Vec<_> = pair.iter::<Rect>().map(rect).collect();
            assert_eq!(rects, vec![sorted, sorted], "case {case}");
            let both = dict.get::<Array<'_>>(b"V").unwrap();
            let arrays: Vec<[f32; 4]> = both.iter::<[f32; 4]>().collect();
            assert_eq!(arrays.len(), 2, "case {case}");
            let own = dict.get::<Array<'_>>(b"R").unwrap();
            let mut flex = own.flex_iter();
            let (a, b) = flex.next::<(f32, f32)>().unwrap();
            let (c, d) = flex.next::<(f32, f32)>().unwrap();
            assert_eq!(vec![a, b, c, d], numbers, "case {case}");
        }
        assert_eq!(LEXED.get(), 0, "case {case}: {holder}");
        // Objects 4 and 5, each once.
        assert!(
            BODIES_READ.get() <= holder.len() + array.len() + 32,
            "case {case}"
        );
    }
}

/// The catalog, the page tree and page 3, `size` points square, whose `/UserUnit`
/// is `unit`.
fn one_page(size: u32, unit: &str) -> Vec<(u32, Vec<u8>)> {
    vec![
        (1, CATALOG.to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (
            3,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {size} {size}] /UserUnit {unit} >>"
            )
            .into_bytes(),
        ),
    ]
}

/// Page 3's `/UserUnit`, and what reading it lost.
fn unit(file: Vec<u8>) -> (Option<f32>, Option<ObjectIdentifier>) {
    let pdf = Pdf::new(file).unwrap();
    let page = &pdf.pages()[0];
    pdf.xref().watch(|| page.raw().get::<f32>(b"UserUnit"))
}

fn id(number: i32) -> ObjectIdentifier {
    ObjectIdentifier::new(number, 0)
}

/// Page 3, whose `/UserUnit` is object 5 of object stream 4, whose `data` put objects 5
/// and 6 after /First 8.
fn cut_in_an_object_stream(data: &str) -> Vec<u8> {
    let mut objects = one_page(50, "5 0 R");
    objects.push((
        4,
        format!(
            "<< /Type /ObjStm /N 2 /First 8 /Length {} >>\nstream\n{data}\nendstream",
            data.len()
        )
        .into_bytes(),
    ));
    stream_table(&objects, &[(5, 4, 0), (6, 4, 1)], false)
}

/// Page 3, whose `/UserUnit` is object 4, `4 0 obj\n2`, then `between`, then `5 0 obj`,
/// with an entry for each header.
fn cut_in_the_file(between: &str) -> Vec<u8> {
    let extra = format!("4 0 obj\n2{between}5 0 obj\n0\nendobj");
    let five = extra.find("5 0 obj").unwrap();
    table_file(
        &one_page(10, "4 0 R"),
        extra.as_bytes(),
        &[(4, 0), (5, five)],
        false,
    )
}

// The next place cuts a token where no delimiter ends it: the body would go on past
// its slot, so it is lost, not read as the token's start. MuPDF reads `1` for the
// `12` of an object stream, and fails object 4 of the file, `2` where the bytes say
// `25`, reading /UserUnit as missing.
#[test]
fn a_token_the_next_place_cuts_is_lost_not_read_as_its_start() {
    // Object 5 at `1` of `12`, object 6 at its `2`.
    let file = cut_in_an_object_stream("5 0 6 1 12");
    assert_eq!(unit(file.clone()), (None, Some(id(5))));
    let pdf = Pdf::new(file).unwrap();
    assert_eq!(pdf.xref().get::<i32>(id(6)), Some(2));
    // A name, `/Ab`, cut after `/A`.
    let file = cut_in_an_object_stream("5 0 6 2 /Ab");
    assert_eq!(unit(file), (None, Some(id(5))));
    // Where white space ends the token, it is read.
    assert_eq!(
        unit(cut_in_an_object_stream("5 0 6 2 1 2")),
        (Some(1.0), None)
    );
    assert_eq!(
        unit(cut_in_an_object_stream("5 0 6 2 12")),
        (Some(12.0), None)
    );
    // In the file, entry 5 at the `5` of `25 0 obj`: the table is rebuilt, and
    // object 4, empty in its slot, lost.
    assert_eq!(unit(cut_in_the_file("")), (None, Some(id(4))));
    assert_eq!(unit(cut_in_the_file(" ")), (Some(2.0), None));
}

/// What loading `file`, whose page 3 is 600 pt square, and reading its `/UserUnit`
/// finds, with what reading it lost; and the bytes lexed and decoded, the object
/// streams' entries read, and the most bytes held at once doing so.
fn loaded_one(file: Vec<u8>) -> ((Option<f32>, Option<ObjectIdentifier>), [usize; 3]) {
    BODIES_READ.set(0);
    LEXED.set(0);
    OBJECT_STREAM_ENTRIES.set(0);
    let (found, used) = measure(move || {
        let pdf = Pdf::new(file).unwrap();
        let page = &pdf.pages()[0];
        assert_eq!((page.media_box().x1, page.media_box().y1), (600.0, 600.0));
        pdf.xref().watch(|| page.raw().get::<f32>(b"UserUnit"))
    });
    let work = [
        BODIES_READ.get() + LEXED.get(),
        OBJECT_STREAM_ENTRIES.get(),
        used.peak,
    ];
    (found, work)
}

/// Asserts that each of the costs of 2 `n` is at most about twice that of `n`.
fn linear_each(family: &str, cost: impl Fn(u32) -> [usize; 3]) {
    let [small, large] = [128, 256].map(&cost);
    assert!(
        small
            .iter()
            .zip(&large)
            .all(|(s, l)| *l <= s * 21 / 10 + 4096),
        "{family}: (bytes lexed and decoded, entries, peak bytes held) {small:?} for 128, \
         {large:?} for 256"
    );
}

/// `n` object streams that one header, `10 0 obj`, heads each, one after another, the
/// data of each, `5 0 7`, reaching over those after it to one pad of `512 n` spaces
/// and the one `endstream`; `startxref` leads to no table, which is rebuilt.
fn object_streams_of_one_number(n: u32) -> Vec<u8> {
    let mut extra =
        b"10 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length 0 >>\nstream\n5 0 7 ".repeat(n as usize);
    extra.extend(b" ".repeat(512 * n as usize));
    extra.extend(b"\nendstream\nendobj");
    table_file(&one_page(600, "5 0 R"), &extra, &[], true)
}

/// One header, `4 0 obj`, then `n` object stream dictionaries, each with its data,
/// `5 0 1`, all reaching over those after it to one tail of `512 n` bytes and the
/// one `endstream`, as each /Length says; the table is rebuilt.
fn object_streams_under_one_header(n: u32) -> Vec<u8> {
    let mut extra = b"4 0 obj\n".to_vec();
    let mut lengths = vec![];
    for _ in 0..n {
        extra.extend(b"<< /Type /ObjStm /N 1 /First 4 /Length LLLLLLLLLL >>\nstream\n");
        lengths.push(extra.len());
        extra.extend(b"5 0 1\n");
    }
    extra.extend(b"A".repeat(512 * n as usize));
    let end = extra.len();
    extra.extend(b"\nendstream\nendobj");
    for data in lengths {
        let at = data - b" >>\nstream\n".len() - 10;
        extra[at..at + 10].copy_from_slice(format!("{:10}", end - data).as_bytes());
    }
    table_file(&one_page(600, "5 0 R"), &extra, &[], true)
}

// A rebuilt table reads the object streams it finds by their places, through the
// body layer: each is the body of its header, its data cut at the next place that
// holds a stream, decoded once, whichever definition of its number the table keeps
// and however many dictionaries follow the header; so what is decoded and kept, and
// the entries read, are linear in the file. The object is the last stream's.
#[test]
fn object_streams_a_rebuilt_table_finds_are_read_in_linear_work_and_memory() {
    assert_eq!(
        loaded_one(object_streams_of_one_number(64)).0,
        (Some(7.0), None)
    );
    linear_each("object streams of one number", |n| {
        loaded_one(object_streams_of_one_number(n)).1
    });
    assert_eq!(
        loaded_one(object_streams_under_one_header(64)).0,
        (Some(1.0), None)
    );
    linear_each("object streams under one header", |n| {
        loaded_one(object_streams_under_one_header(n)).1
    });
}

/// Page 3, 600 by 700 pt, and `n` catalogs, objects 100 on, in one Flate object
/// stream with a tail of `4096 n` spaces, and a trailer naming each; the table is
/// rebuilt.
fn catalogs_in_one_object_stream(n: u32) -> Vec<u8> {
    use std::io::Write;
    let mut header = String::new();
    let mut body = String::new();
    for k in 0..n {
        header += &format!("{} {} ", 100 + k, body.len());
        body += std::str::from_utf8(CATALOG).unwrap();
        body += " ";
    }
    let mut data = (header.clone() + &body).into_bytes();
    data.extend(b" ".repeat(4096 * n as usize));
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&data).unwrap();
    let z = z.finish().unwrap();
    let mut file = b"%PDF-1.7\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
        3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] >>\nendobj\n"
        .to_vec();
    file.extend(
        format!(
            "4 0 obj\n<< /Type /ObjStm /N {n} /First {} /Filter /FlateDecode /Length {} >>\nstream\n",
            header.len(),
            z.len()
        )
        .bytes(),
    );
    file.extend(z);
    file.extend(b"\nendstream\nendobj\n");
    for k in 0..n {
        file.extend(format!("trailer\n<< /Size 200 /Root {} 0 R >>\n", 100 + k).bytes());
    }
    file.extend(b"startxref\n9\n%%EOF\n");
    file
}

// Every trailer a rebuild finds names a catalog in one object stream: the catalogs
// are read as the rebuilt table reads them, the stream decoded once and each catalog
// read once, so checking them takes work and memory linear in the file. The last
// trailer is taken.
#[test]
fn catalogs_in_one_object_stream_are_checked_in_linear_work_and_memory() {
    let cost = |n: u32| {
        BODIES_READ.set(0);
        LEXED.set(0);
        OBJECT_STREAM_ENTRIES.set(0);
        let file = catalogs_in_one_object_stream(n);
        let (root, used) = measure(move || {
            let pdf = Pdf::new(file).unwrap();
            let page = &pdf.pages()[0];
            assert_eq!((page.media_box().x1, page.media_box().y1), (600.0, 700.0));
            pdf.xref().root_id()
        });
        assert_eq!(root, id(100 + n as i32 - 1));
        [
            BODIES_READ.get() + LEXED.get(),
            OBJECT_STREAM_ENTRIES.get(),
            used.peak,
        ]
    };
    linear_each("catalogs in one object stream", cost);
}

/// Page 3, whose `/UserUnit` is object 5, the one object of object stream 10, whose
/// data, `5 0 12` and `after`, reach over stream 9 after them, as its /Length says.
fn cut_at_another_stream(after: &str) -> Vec<u8> {
    let inner = "9 0 obj\n<< /Length 3 >>\nstream\nabc\nendstream\nendobj\n";
    let data = format!("5 0 12{after}{inner}");
    let mut objects = one_page(50, "5 0 R");
    objects.push((
        10,
        format!(
            "<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n{data}\nendstream",
            data.len()
        )
        .into_bytes(),
    ));
    let mut file = stream_table(&objects, &[(5, 10, 0)], false);
    // The table, object 11, puts stream 9 at its header, in stream 10's data.
    let at = file.windows(7).position(|w| w == b"9 0 obj").unwrap() as u32;
    let table = file.windows(11).position(|w| w == b"/Type /XRef").unwrap();
    let rows = table
        + file[table..]
            .windows(7)
            .position(|w| w == b"stream\n")
            .unwrap()
        + 7;
    let row = rows + 9 * 7;
    assert_eq!(file[row], 0, "no entry 9 yet");
    file[row] = 1;
    file[row + 1..row + 5].copy_from_slice(&at.to_be_bytes());
    file
}

// An object stream's data cut at the place of a stream they reach end where the
// stream's go on: the last object, reaching that end, may go on past it, so it is
// lost, not read as its start (`12` of the `129` the stream's data hold).
#[test]
fn an_object_that_reaches_the_cut_of_its_stream_is_lost() {
    assert_eq!(unit(cut_at_another_stream("")), (None, Some(id(5))));
    assert_eq!(unit(cut_at_another_stream(" ")), (Some(12.0), None));
}

/// Page 3, whose `/UserUnit` is object 5, at the start of `extra`, with entry 6 at
/// `six` in it.
fn entry_before(extra: &[u8], six: usize) -> Vec<u8> {
    table_file(&one_page(600, "5 0 R"), extra, &[(5, 0), (6, six)], false)
}

// A header is read in its slot, as a body is: an entry whose header the next offset
// cuts, or whose bytes start like a header up to it, is lost, as what it names is not
// known there, and reading on would read the next place's bytes again for each such
// entry. Bytes that are no header in the slot are no object. MuPDF reads `7 0 x` as no
// object, whatever the next offset, and uses the default scale.
#[test]
fn an_entry_whose_header_the_next_offset_cuts_is_lost() {
    // Entry 6 at the `2` of `12 0 obj`.
    assert_eq!(
        unit(entry_before(b"12 0 obj\n3\nendobj", 1)),
        (None, Some(id(5)))
    );
    // Entry 6 at the space after `7 0`, which a header may go on after.
    assert_eq!(
        unit(entry_before(b"7 0 x 6 0 obj\n2\nendobj", 3)),
        (None, Some(id(5)))
    );
    // Entry 6 at the `x`, which the slot ends at: no header.
    assert_eq!(
        unit(entry_before(b"7 0 x 6 0 obj\n2\nendobj", 4)),
        (None, None)
    );
    // No offset in the header: it is read.
    assert_eq!(
        unit(entry_before(b"5 0 obj\n3\nendobj\n6 0 obj 1 endobj", 17)),
        (Some(3.0), None)
    );
}
