//! When a file opens, whether its cross-reference table is kept or rebuilt from a
//! scan of the file, and what a rebuilt table holds, observed only through the
//! crate's public API: `Pdf::new`, `XRef::get` within `XRef::watch`, and the pages.
//! The rules these tests pin are stated in the doc of the crate's `xref` module.
//!
//! An object is shown as `N=value` ([`seen`]): its value (a number, `(string)`,
//! `[items]`, `<</Type>>` for a dictionary, `stream`), `-` where the table does not
//! list it (a reference to it is null and nothing is lost), `!` where the table
//! lists it but it cannot be read (reading it tells the computation being watched
//! that it lost the object; `!N` where it tells object `N` instead). Object 9 is in
//! the body of every file and in none of its tables: `9=90` shows that the table was
//! rebuilt from a scan of the file, `9=-` that it was kept.
//!
//! With `XREF_CHECK_PDFS=<dir>` set, each file a test reads is written there under
//! the test's and the case's name. `PyMuPDF` 1.28.2 was run on them, reading each
//! object with `Document.xref_object` and noting `Document.is_repaired` at open
//! and after the reads; where a case says nothing of `PyMuPDF`, it shows the same
//! values. Differences are named per case. Two hold throughout:
//!
//! - `MuPDF` checks a table at open only for entries past the end of the file,
//!   object stream entries whose stream it does not list, and a trailer without
//!   /Size or with one below the entries; it repairs on demand where a read finds
//!   another header than the entry's, so a value read before that repair may
//!   differ from one read after it. Here the table is rebuilt at open, before any
//!   object is read, so every read is from one table.
//! - `MuPDF` reads at an entry's offset past white space and comments; here the
//!   header must be at the offset, else the table is rebuilt (which finds the same
//!   object there unless the file defines its number twice).

use hayro_syntax::Pdf;
use hayro_syntax::object::{Name, Object, ObjectIdentifier};

/// A row of a cross-reference section.
#[derive(Clone, Copy, Debug)]
enum Row {
    /// In use at this offset, generation 0.
    At(usize),
    /// In use at this offset, with this generation.
    Gen(usize, u16),
    /// Free.
    Free,
    /// In the object stream of this number, at this index (cross-reference streams
    /// only).
    In(u32, u32),
}

/// The /Size of a trailer: one past the highest number its section lists, none, or
/// this.
#[derive(Clone, Copy)]
enum Size {
    Auto,
    Omitted,
    Is(i32),
}

/// An offset past the end of every file here.
const PAST_END: usize = 1_000_000;

/// The object every file holds and no table lists ([`seen`]).
const UNLISTED: i32 = 9;

/// An entry past the end of the file, of an object no file here holds, which
/// rebuilds the table it is in.
const DAMAGED: (i32, Row) = (8, Row::At(PAST_END));

const CATALOG: &str = "<< /Type /Catalog /Pages 2 0 R >>";
const NO_PAGES: &str = "<< /Type /Pages /Kids [] /Count 0 >>";

/// A file being written: a header, then objects and sections in order.
struct Doc {
    out: Vec<u8>,
}

impl Doc {
    fn new() -> Self {
        Self {
            out: b"%PDF-1.7\n".to_vec(),
        }
    }

    fn at(&self) -> usize {
        self.out.len()
    }

    /// Writes object `number` holding `body`, returning where its header is.
    fn obj(&mut self, number: i32, body: &str) -> usize {
        let at = self.at();
        self.raw(&format!("{number} 0 obj\n{body}\nendobj\n"));
        at
    }

    fn raw(&mut self, text: &str) {
        self.out.extend(text.bytes());
    }

    /// Where `needle` first is from `from` on.
    fn find(&self, from: usize, needle: &str) -> usize {
        let needle = needle.as_bytes();
        from + self.out[from..]
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap()
    }

    /// Writes a table of `rows`, each a subsection of its own, and its trailer with
    /// `size`, `/Root 1 0 R` and `trailer` entries, returning where it is.
    fn table(&mut self, rows: &[(i32, Row)], size: Size, trailer: &str) -> usize {
        let at = self.at();
        self.raw("xref\n0 1\n0000000000 65535 f \n");
        for (number, row) in rows {
            let line = match row {
                Row::At(offset) => format!("{offset:010} 00000 n \n"),
                Row::Gen(offset, generation) => format!("{offset:010} {generation:05} n \n"),
                Row::Free => "0000000000 00001 f \n".to_owned(),
                Row::In(..) => unreachable!("a table holds no entry into an object stream"),
            };
            self.raw(&format!("{number} 1\n{line}"));
        }
        let size = match size {
            Size::Auto => {
                let highest = rows.iter().map(|(n, _)| *n).max().unwrap_or(0);
                format!("/Size {} ", highest + 1)
            }
            Size::Omitted => String::new(),
            Size::Is(size) => format!("/Size {size} "),
        };
        self.raw(&format!("trailer\n<< {size}/Root 1 0 R {trailer} >>\n"));
        at
    }

    /// Writes a cross-reference stream, object `number`, listing `rows` and itself,
    /// returning where it is.
    fn stream_table(&mut self, number: i32, rows: &[(i32, Row)]) -> usize {
        let at = self.at();
        let mut rows = rows.to_vec();
        rows.push((number, Row::At(at)));
        rows.sort_by_key(|(n, _)| *n);
        let mut data = vec![];
        let mut index = String::new();
        for (n, row) in &rows {
            index += &format!("{n} 1 ");
            let (kind, field, extra) = match *row {
                Row::At(offset) => (1_u8, offset as u32, 0_u16),
                Row::Gen(offset, generation) => (1, offset as u32, generation),
                Row::Free => (0, 0, 1),
                Row::In(stream, i) => (2, stream, i as u16),
            };
            data.push(kind);
            data.extend(field.to_be_bytes());
            data.extend(extra.to_be_bytes());
        }
        let size = rows.last().unwrap().0 + 1;
        self.raw(&format!(
            "{number} 0 obj\n<< /Type /XRef /Size {size} /W [1 4 2] /Index [{index}] \
             /Root 1 0 R /Length {} >>\nstream\n",
            data.len()
        ));
        self.out.extend(data);
        self.raw("\nendstream\nendobj\n");
        at
    }

    fn startxref(&mut self, at: usize) {
        self.raw(&format!("startxref\n{at}\n%%EOF\n"));
    }
}

/// A file of the catalog 1, an empty page tree 2, `objects` (each a number and a
/// body) in this order, and object 9, `90`; whose table, ending the file, lists 1, 2
/// and the rows `rows` gives from where the header of each of `objects` is.
fn file_with(
    objects: &[(i32, &str)],
    rows: impl Fn(&[usize]) -> Vec<(i32, Row)>,
    size: Size,
    trailer: &str,
) -> Vec<u8> {
    let mut d = Doc::new();
    let catalog = d.obj(1, CATALOG);
    let tree = d.obj(2, NO_PAGES);
    let at: Vec<usize> = objects.iter().map(|(n, body)| d.obj(*n, body)).collect();
    d.obj(UNLISTED, "90");
    let mut all = vec![(1, Row::At(catalog)), (2, Row::At(tree))];
    all.extend(rows(&at));
    let table = d.table(&all, size, trailer);
    d.startxref(table);
    d.out
}

/// [`file_with`] the usual /Size and no other trailer entry.
fn file(objects: &[(i32, &str)], rows: impl Fn(&[usize]) -> Vec<(i32, Row)>) -> Vec<u8> {
    file_with(objects, rows, Size::Auto, "")
}

/// As [`file`], the table a cross-reference stream (object 12); where not `table`,
/// `startxref` leads to the catalog's header, where no section can be read.
fn streamed(
    objects: &[(i32, String)],
    rows: impl Fn(&[usize]) -> Vec<(i32, Row)>,
    table: bool,
) -> Vec<u8> {
    let mut d = Doc::new();
    let catalog = d.obj(1, CATALOG);
    let tree = d.obj(2, NO_PAGES);
    let at: Vec<usize> = objects.iter().map(|(n, body)| d.obj(*n, body)).collect();
    d.obj(UNLISTED, "90");
    let mut all = vec![(1, Row::At(catalog)), (2, Row::At(tree))];
    all.extend(rows(&at));
    let at = d.stream_table(12, &all);
    d.startxref(if table { at } else { catalog });
    d.out
}

/// An object stream holding `objects`, each a number and a body, in this order.
fn object_stream(objects: &[(u32, &str)]) -> String {
    let mut header = String::new();
    let mut body = String::new();
    for (number, object) in objects {
        header += &format!("{number} {} ", body.len());
        body += object;
        body.push('\n');
    }
    let data = format!("{header}{body}");
    format!(
        "<< /Type /ObjStm /N {} /First {} /Length {} >>\nstream\n{data}\nendstream",
        objects.len(),
        header.len(),
        data.len()
    )
}

/// A stream holding `data`.
fn stream(data: &str) -> String {
    format!("<< /Length {} >>\nstream\n{data}\nendstream", data.len())
}

fn show(object: &Object<'_>) -> String {
    match object {
        Object::Null(_) => "null".to_owned(),
        Object::Boolean(b) => b.to_string(),
        Object::Number(n) => n.as_f64().to_string(),
        Object::String(s) => format!("({})", String::from_utf8_lossy(s.as_bytes())),
        Object::Name(n) => format!("/{}", n.as_str()),
        Object::Array(a) => {
            let items: Vec<String> = a.iter::<Object<'_>>().map(|o| show(&o)).collect();
            format!("[{}]", items.join(" "))
        }
        Object::Dict(d) => match d.get::<Name<'_>>(b"Type") {
            Some(t) => format!("<</{}>>", t.as_str()),
            None => "<<>>".to_owned(),
        },
        Object::Stream(_) => "stream".to_owned(),
    }
}

/// What reading `id` of `pdf` finds, as the module doc shows it.
fn value_of(pdf: &Pdf, id: ObjectIdentifier) -> String {
    let xref = pdf.xref();
    let (object, lost) = xref.watch(|| xref.get::<Object<'_>>(id));
    match (object, lost) {
        (_, Some(lost)) if lost == id => "!".to_owned(),
        (_, Some(lost)) => format!("!{}", lost.obj_number),
        (None, None) => "-".to_owned(),
        (Some(object), None) => show(&object),
    }
}

/// The objects `numbers` of `pdf`, generation 0, as the module doc shows them.
fn values(pdf: &Pdf, numbers: &[i32]) -> String {
    numbers
        .iter()
        .map(|n| format!("{n}={}", value_of(pdf, ObjectIdentifier::new(*n, 0))))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Writes `file` as `name` where `XREF_CHECK_PDFS` names a directory.
fn dump(name: &str, file: &[u8]) {
    if let Some(dir) = std::env::var_os("XREF_CHECK_PDFS") {
        let name = name.replace(|c: char| !c.is_ascii_alphanumeric(), "_");
        std::fs::write(std::path::Path::new(&dir).join(format!("{name}.pdf")), file).unwrap();
    }
}

fn open(name: &str, file: Vec<u8>) -> Pdf {
    dump(name, &file);
    Pdf::new(file).unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

/// The objects `numbers` and 9 of `file`, which `name` names, as the module doc
/// shows them.
fn seen(name: &str, file: Vec<u8>, numbers: &[i32]) -> String {
    let mut numbers = numbers.to_vec();
    numbers.push(UNLISTED);
    values(&open(name, file), &numbers)
}

/// A case: a name, a file, the objects to read, and what they read.
type Case<'a> = (&'a str, Vec<u8>, &'a [i32], &'a str);

fn expect(cases: Vec<Case<'_>>) {
    for (name, file, numbers, expected) in cases {
        assert_eq!(seen(name, file, numbers), expected, "{name}");
    }
}

/// The rows of `numbers` at `at`, in order.
fn own(numbers: &[i32], at: &[usize]) -> Vec<(i32, Row)> {
    numbers
        .iter()
        .zip(at)
        .map(|(n, at)| (*n, Row::At(*at)))
        .collect()
}

// Every entry leads to a header naming its object, at its offset and ending before
// the next offset any entry names: the table is kept, whatever follows the header.
#[test]
fn a_table_whose_entries_lead_to_their_headers_is_kept() {
    let three = [(3, "30")];
    expect(vec![
        ("sound", file(&three, |at| own(&[3], at)), &[3], "3=30 9=-"),
        // `PyMuPDF` repairs at open where /Size is below the entries or missing: the
        // values are the same.
        (
            "size below the entries",
            file_with(&three, |at| own(&[3], at), Size::Is(2), ""),
            &[3],
            "3=30 9=-",
        ),
        (
            "size above the entries",
            file_with(&three, |at| own(&[3], at), Size::Is(100), ""),
            &[3],
            "3=30 9=-",
        ),
        (
            "no size",
            file_with(&three, |at| own(&[3], at), Size::Omitted, ""),
            &[3],
            "3=30 9=-",
        ),
        // A body that cannot be read is lost when it is read, not a reason to
        // rebuild. `PyMuPDF` does not repair either, and reads `<<>>`.
        (
            "a body that cannot be read",
            file(&[(3, "<< /A ] >>")], |at| own(&[3], at)),
            &[3],
            "3=! 9=-",
        ),
        // Nor a body that runs past the next offset, here into a string holding the
        // header 4's entry leads to: 3 is lost. `PyMuPDF` reads 3 whole.
        (
            "a body past the next offset",
            nested(false),
            &[3, 4],
            "3=! 4=(y) 9=-",
        ),
        // An entry into an object stream the table lists as an object of the file.
        (
            "in a listed object stream",
            streamed(
                &[(4, object_stream(&[(5, "50")]))],
                |at| vec![(4, Row::At(at[0])), (5, Row::In(4, 0))],
                true,
            ),
            &[5],
            "5=50 9=-",
        ),
        // The stream's object need not be an object stream: reading the entry loses
        // it. `PyMuPDF` does not repair either, and fails the read.
        (
            "in a listed object that is no stream",
            streamed(
                &[(3, "30".to_owned())],
                |at| vec![(3, Row::At(at[0])), (5, Row::In(3, 0))],
                true,
            ),
            &[3, 5],
            "3=30 5=!3 9=-",
        ),
        // An entry of an earlier section that a later one replaces is not checked.
        // `PyMuPDF` repairs at open, as it checks every section's entries: the
        // values are the same.
        (
            "a replaced entry past the end",
            update(&[(3, Row::At(PAST_END)), (4, BASE)], &new_three, None),
            &[3, 4],
            "3=33 4=40 9=-",
        ),
        // `PyMuPDF` reads 5 as null where the table lists it as free, as here, and
        // `50` where the table does not list it.
        ("a hybrid file", hybrid(None, false), &[5], "5=50 9=-"),
        // Where the table lists 5 in use, its entry replaces the stream's.
        (
            "a hybrid file listing 5 in both",
            hybrid(None, true),
            &[5],
            "5=51 9=-",
        ),
    ]);
}

/// Object 3 holding, in a string, the header of 4, where 4's entry leads; with
/// `damaged`, the table also has [`DAMAGED`].
fn nested(damaged: bool) -> Vec<u8> {
    let mut d = Doc::new();
    let catalog = d.obj(1, CATALOG);
    let tree = d.obj(2, NO_PAGES);
    let three = d.obj(3, "(x 4 0 obj (y) endobj z)");
    let four = d.find(three + 1, "4 0 obj");
    d.obj(UNLISTED, "90");
    let mut rows = vec![
        (1, Row::At(catalog)),
        (2, Row::At(tree)),
        (3, Row::At(three)),
        (4, Row::At(four)),
    ];
    if damaged {
        rows.push(DAMAGED);
    }
    let table = d.table(&rows, Size::Auto, "");
    d.startxref(table);
    d.out
}

/// The row of an object of [`update`]'s first section at its header.
const BASE: Row = Row::At(usize::MAX);

/// A file of 3 (`30`) and 4 (`40`), whose first section lists them as `base` says,
/// [`BASE`] at their headers, and an update the rows of whose section `update`
/// writes, whose /Prev is `prev` or the first section.
fn update(
    base: &[(i32, Row)],
    update: &dyn Fn(&mut Doc) -> Vec<(i32, Row)>,
    prev: Option<usize>,
) -> Vec<u8> {
    let mut d = Doc::new();
    let catalog = d.obj(1, CATALOG);
    let tree = d.obj(2, NO_PAGES);
    let three = d.obj(3, "30");
    let four = d.obj(4, "40");
    d.obj(UNLISTED, "90");
    let mut rows = vec![(1, Row::At(catalog)), (2, Row::At(tree))];
    for (number, row) in base {
        let row = match (number, row) {
            (3, Row::At(usize::MAX)) => Row::At(three),
            (4, Row::At(usize::MAX)) => Row::At(four),
            _ => *row,
        };
        rows.push((*number, row));
    }
    let first = d.table(&rows, Size::Is(10), "");
    d.startxref(first);
    let rows = update(&mut d);
    let prev = prev.unwrap_or(first);
    let second = d.table(&rows, Size::Is(10), &format!("/Prev {prev}"));
    d.startxref(second);
    d.out
}

/// An update that writes 3 again, `33`, and lists it there.
fn new_three(d: &mut Doc) -> Vec<(i32, Row)> {
    vec![(3, Row::At(d.obj(3, "33")))]
}

/// A hybrid file: a table listing the object stream 4, which holds 5, and 5 as free,
/// or, where `direct`, at a definition of 5 outside it, `51`; whose `/XRefStm`, at
/// `stm` or the stream written, lists 5 in 4.
fn hybrid(stm: Option<usize>, direct: bool) -> Vec<u8> {
    let mut d = Doc::new();
    let catalog = d.obj(1, CATALOG);
    let tree = d.obj(2, NO_PAGES);
    let four = d.obj(4, &object_stream(&[(5, "50")]));
    let five = if direct {
        Row::At(d.obj(5, "51"))
    } else {
        Row::Free
    };
    d.obj(UNLISTED, "90");
    let written = d.stream_table(12, &[(5, Row::In(4, 0))]);
    let rows = [
        (1, Row::At(catalog)),
        (2, Row::At(tree)),
        (4, Row::At(four)),
        (5, five),
    ];
    let stm = stm.unwrap_or(written);
    let table = d.table(&rows, Size::Is(13), &format!("/XRefStm {stm}"));
    d.startxref(table);
    d.out
}

// A table with an entry that does not lead to a header naming its object at its
// offset, or into an object stream it does not list, or that cannot be read, is
// rebuilt from a scan of the file.
#[test]
fn a_table_with_an_entry_that_does_not_lead_to_its_header_is_rebuilt() {
    let three = [(3, "30")];
    let at_three =
        |delta: isize| move |at: &[usize]| vec![(3, Row::At(at[0].wrapping_add_signed(delta)))];
    let comment_before = {
        let mut d = Doc::new();
        let catalog = d.obj(1, CATALOG);
        let tree = d.obj(2, NO_PAGES);
        let comment = d.at();
        d.raw("% a comment\n");
        d.obj(3, "30");
        d.obj(UNLISTED, "90");
        let rows = [
            (1, Row::At(catalog)),
            (2, Row::At(tree)),
            (3, Row::At(comment)),
        ];
        let table = d.table(&rows, Size::Auto, "");
        d.startxref(table);
        d.out
    };
    let split_header = {
        let mut d = Doc::new();
        let catalog = d.obj(1, CATALOG);
        let tree = d.obj(2, NO_PAGES);
        let three = d.at();
        d.raw("3 0 % 4 0 obj\nobj\n30\nendobj\n");
        let four = d.find(three + 1, "4 0 obj");
        d.obj(UNLISTED, "90");
        let rows = [
            (1, Row::At(catalog)),
            (2, Row::At(tree)),
            (3, Row::At(three)),
            (4, Row::At(four)),
        ];
        let table = d.table(&rows, Size::Auto, "");
        d.startxref(table);
        d.out
    };
    let updated_into_its_header = |d: &mut Doc| {
        let at = d.obj(3, "33");
        vec![(3, Row::At(at + 1))]
    };
    let in_four = || vec![(4, object_stream(&[(5, "50")]))];
    expect(vec![
        // `PyMuPDF` repairs at open.
        (
            "past the end",
            file(&three, |_| vec![(3, Row::At(PAST_END))]),
            &[3],
            "3=30 9=90",
        ),
        // `PyMuPDF` repairs when 3 is read.
        (
            "a byte into its header",
            file(&three, at_three(1)),
            &[3],
            "3=30 9=90",
        ),
        (
            "two bytes into its header",
            file(&three, at_three(2)),
            &[3],
            "3=30 9=90",
        ),
        // `PyMuPDF` reads past white space and comments, and does not repair.
        (
            "at white space before its header",
            file(&three, at_three(-1)),
            &[3],
            "3=30 9=90",
        ),
        (
            "at a comment before its header",
            comment_before,
            &[3],
            "3=30 9=90",
        ),
        // Into the body of 3, where no header is.
        (
            "into a body",
            file(&[(3, "[1 2 3 4 5]")], |at| {
                vec![(3, Row::At(at[0])), (4, Row::At(at[0] + 12))]
            }),
            &[3, 4],
            "3=[1 2 3 4 5] 4=- 9=90",
        ),
        // Into 3's header, whose slot that offset cuts.
        (
            "into another entry's header",
            file(&three, |at| {
                vec![(3, Row::At(at[0])), (4, Row::At(at[0] + 2))]
            }),
            &[3, 4],
            "3=30 4=- 9=90",
        ),
        // A header that a comment splits, holding the header 4's entry leads to:
        // whole, 3's header names 3, but it does not end before 4's offset, so the
        // table is rebuilt. The scan does not find 4; its entry leads into 3's
        // header, so to that header (rule 7), and is kept there, `30` (rule 8).
        // 3's old entry leads to a header 4's offset cuts, so 3 is lost (rule 9).
        // `PyMuPDF` does not repair, reads 3 as `30` and fails to read 4.
        (
            "a header the next offset cuts",
            split_header,
            &[3, 4],
            "3=! 4=30 9=90",
        ),
        // `PyMuPDF` repairs at open.
        (
            "in an object stream the table does not list",
            streamed(
                &in_four(),
                |at| vec![(4, Row::At(at[0])), (5, Row::In(7, 0))],
                true,
            ),
            &[5],
            "5=50 9=90",
        ),
        (
            "in an object stream the table lists as free",
            streamed(
                &in_four(),
                |_| vec![(4, Row::Free), (5, Row::In(4, 0))],
                true,
            ),
            &[5],
            "5=50 9=90",
        ),
        (
            "no section where startxref leads",
            streamed(&[(3, "30".to_owned())], |at| own(&[3], at), false),
            &[3],
            "3=30 9=90",
        ),
        (
            "no section where /Prev leads",
            update(&[(3, BASE), (4, BASE)], &new_three, Some(PAST_END)),
            &[3, 4],
            "3=33 4=40 9=90",
        ),
        (
            "an entry of an earlier section past the end",
            update(&[(3, BASE), (4, Row::At(PAST_END))], &new_three, None),
            &[3, 4],
            "3=33 4=40 9=90",
        ),
        (
            "an update's entry into its header",
            update(&[(3, BASE), (4, BASE)], &updated_into_its_header, None),
            &[3, 4],
            "3=33 4=40 9=90",
        ),
        (
            "no section where /XRefStm leads",
            hybrid(Some(PAST_END), false),
            &[5],
            "5=50 9=90",
        ),
        // A trailer whose /Root is no dictionary. `PyMuPDF` does not repair to read
        // objects.
        (
            "a root that is no dictionary",
            file_with(&three, |at| own(&[3], at), Size::Auto, "/Root 3 0 R"),
            &[3],
            "3=30 9=90",
        ),
    ]);
}

// The table is rebuilt at open, before any object is read, so every read is from
// the rebuilt table. Here entry 4 leads to the header of 9, and the table lists the
// first of two definitions of 3: 3 is the later one. `PyMuPDF` repairs only when 4
// is read: 3 reads `31` before that and `32` after it; 4 reads null, 9 `90`.
#[test]
fn a_table_is_rebuilt_before_any_object_is_read() {
    let mut d = Doc::new();
    let catalog = d.obj(1, CATALOG);
    let tree = d.obj(2, NO_PAGES);
    let first = d.obj(3, "31");
    d.obj(3, "32");
    let nine = d.obj(UNLISTED, "90");
    let rows = [
        (1, Row::At(catalog)),
        (2, Row::At(tree)),
        (3, Row::At(first)),
        (4, Row::At(nine)),
    ];
    let table = d.table(&rows, Size::Auto, "");
    d.startxref(table);
    let pdf = open("rebuilt at open", d.out);
    assert_eq!(values(&pdf, &[3]), "3=32");
    assert_eq!(values(&pdf, &[4, 3, 9]), "4=90 3=32 9=-");
}

/// A file whose body holds `objects` in order, each a number and a body, a number
/// possibly twice, and whose table lists each number at its first definition; at
/// `objects[at]` for `(number, at)` in `misplaced`; and not at all for a number
/// in `unlisted`.
fn listed_at(objects: &[(i32, &str)], misplaced: &[(i32, usize)], unlisted: &[i32]) -> Vec<u8> {
    let mut d = Doc::new();
    let mut first = std::collections::BTreeMap::new();
    let mut offsets = vec![];
    for (number, body) in objects {
        let at = d.obj(*number, body);
        first.entry(*number).or_insert(at);
        offsets.push(at);
    }
    let listed = misplaced.iter().map(|(number, _)| number);
    let size = first.keys().chain(listed).max().unwrap() + 1;
    let mut rows = vec![];
    for number in 1..size {
        let offset = match misplaced.iter().find(|(n, _)| *n == number) {
            Some((_, at)) => Some(offsets[*at]),
            None => first.get(&number).copied(),
        };
        if let Some(offset) = offset.filter(|_| !unlisted.contains(&number)) {
            rows.push((number, Row::At(offset)));
        }
    }
    let table = d.table(&rows, Size::Is(size), "");
    d.startxref(table);
    d.out
}

/// Each page of `file`: the width of its media box, its rotation in quarter turns
/// and the names of its `/XObject` resources.
fn inherited_in(name: &str, file: Vec<u8>) -> Vec<(f64, u8, Vec<String>)> {
    use hayro_syntax::page::Rotation;
    let pdf = open(name, file);
    pdf.pages()
        .iter()
        .map(|page| {
            let quarters = match page.rotation() {
                Rotation::None => 0,
                Rotation::Horizontal => 1,
                Rotation::Flipped => 2,
                Rotation::FlippedHorizontal => 3,
            };
            let names = page
                .resources()
                .x_objects
                .keys()
                .map(|name| name.as_str().to_owned())
                .collect();
            (page.media_box().x1, quarters, names)
        })
        .collect()
}

// A table with an entry that does not lead to its object is rebuilt before any page
// is read, so every page reads the rebuilt table, which takes the later of two
// definitions: here both pages inherit node 3's second box, though the entry that is
// wrong is met only on the second page. `PyMuPDF` 1.28.2 repairs while it loads the
// second page, and sizes both pages by the box it read before: 100 × 100.
#[test]
fn a_damaged_table_is_rebuilt_before_any_page_inherits_from_it() {
    let file = listed_at(
        &[
            (1, CATALOG),
            (2, "<< /Type /Pages /Kids [4 0 R 5 0 R] /Count 2 >>"),
            (3, "<< /Type /Pages /MediaBox [0 0 100 100] >>"),
            (4, "<< /Type /Page /Parent 3 0 R >>"),
            (5, "<< /Type /Page /Parent 3 0 R /Rotate 6 0 R >>"),
            (6, "90"),
            (3, "<< /Type /Pages /MediaBox [0 0 200 200] >>"),
        ],
        &[(6, 4)],
        &[],
    );
    assert_eq!(
        inherited_in("rebuilt before inheriting", file),
        [(200.0, 0, vec![]), (200.0, 1, vec![])]
    );
}

// The root's /Resources, which the table does not list, is found in the rebuilt
// table by every page that inherits it.
#[test]
fn an_object_only_the_rebuilt_table_lists_is_inherited_by_every_page() {
    let file = listed_at(
        &[
            (1, CATALOG),
            (
                2,
                "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /Resources 7 0 R \
                 /MediaBox [0 0 300 300] >>",
            ),
            (3, "<< /Type /Page /Parent 2 0 R >>"),
            (4, "<< /Type /Page /Parent 2 0 R /Rotate 8 0 R >>"),
            (7, "<< /XObject << /A 9 0 R >> >>"),
            (8, "90"),
        ],
        &[(8, 2)],
        &[7],
    );
    let a = vec!["A".to_owned()];
    assert_eq!(
        inherited_in("only the rebuilt table lists", file),
        [(300.0, 0, a.clone()), (300.0, 1, a)]
    );
}

// Of two definitions of a number outside object streams, a rebuilt table takes the
// later in the file; a table that is kept, the one it lists.
#[test]
fn a_rebuilt_table_takes_the_later_of_two_definitions() {
    let twice = [(3, "31"), (3, "32")];
    let twice_owned = [(3, "31".to_owned()), (3, "32".to_owned())];
    expect(vec![
        ("listed", file(&twice, |at| own(&[3], at)), &[3], "3=31 9=-"),
        (
            "rebuilt",
            file(&twice, |at| vec![(3, Row::At(at[0])), DAMAGED]),
            &[3],
            "3=32 9=90",
        ),
        (
            "no table",
            streamed(&twice_owned, |at| own(&[3], at), false),
            &[3],
            "3=32 9=90",
        ),
    ]);
}

// Where the table names one definition of the page tree and the file ends with
// another, the pages are those of the later one, and inherit from it alone.
// `PyMuPDF` 1.28.2 lists the same pages, rotated alike.
#[test]
fn a_root_defined_twice_is_read_from_the_rebuilt_table_alone() {
    let file = listed_at(
        &[
            (1, CATALOG),
            (
                2,
                "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 \
                 /Resources << /XObject << /A 9 0 R >> >> >>",
            ),
            (3, "<< /Type /Page /Rotate 4 0 R /Resources << >> >>"),
            (4, "90"),
            (5, "<< /Type /Page /Parent 2 0 R >>"),
            (2, "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>"),
        ],
        &[(4, 2)],
        &[],
    );
    let a4 = hayro_syntax::page::A4.x1;
    assert_eq!(
        inherited_in("root defined twice", file),
        [(a4, 1, vec![]), (a4, 0, vec![])]
    );
}

// An entry that leads to the header of another object, H, rebuilds the table. Where
// the scan finds an object of the entry's own number, that is the entry's object.
// Else the entry keeps the object it leads to, read as its own; and H, unless the
// table also lists H at that header, takes the last definition the scan finds
// elsewhere, and is not listed where there is none.
//
// `PyMuPDF` 1.28.2 reads such an entry as null and repairs, after which an object
// found under the entry's number is used, else the entry stays null; and H is its
// last definition in the file. So, deliberately, 3 reads 4's object here where
// `PyMuPDF` reads null. Where 4 is defined only at that header and the table does not
// also list 4 there, 4 reads null here, where `PyMuPDF` reads `40`: an object the file
// holds is lost without the read being told.
#[test]
fn an_entry_that_leads_to_another_objects_header_keeps_that_object() {
    let to_four = |four: Option<Row>| {
        move |at: &[usize]| {
            let mut rows = vec![(3, Row::At(at[0]))];
            rows.extend(four.map(|row| (4, row)));
            rows
        }
    };
    let four = [(4, "40")];
    let four_then_three = [(4, "40"), (3, "30")];
    expect(vec![
        (
            "4 listed there too",
            file(&four, |at| vec![(3, Row::At(at[0])), (4, Row::At(at[0]))]),
            &[3, 4],
            "3=40 4=40 9=90",
        ),
        (
            "4 not listed",
            file(&four, to_four(None)),
            &[3, 4],
            "3=40 4=- 9=90",
        ),
        (
            "4 listed past the end",
            file(&four, to_four(Some(Row::At(PAST_END)))),
            &[3, 4],
            "3=40 4=- 9=90",
        ),
        (
            "3 defined elsewhere",
            file(&four_then_three, |at| {
                vec![(3, Row::At(at[0])), (4, Row::At(at[0]))]
            }),
            &[3, 4],
            "3=30 4=40 9=90",
        ),
        (
            "3 defined elsewhere, 4 not listed",
            file(&four_then_three, to_four(None)),
            &[3, 4],
            "3=30 4=40 9=90",
        ),
        // Entry 3 leads to the second of two definitions of 4, 4's entry to the
        // first: 4 is the first. `PyMuPDF`: 3 null, 4 the second.
        (
            "the second of two definitions",
            file(&[(4, "41"), (4, "42")], |at| {
                vec![(3, Row::At(at[1])), (4, Row::At(at[0]))]
            }),
            &[3, 4],
            "3=42 4=41 9=90",
        ),
        (
            "the second of two definitions, 3 defined elsewhere",
            file(&[(4, "41"), (4, "42"), (3, "30")], |at| {
                vec![(3, Row::At(at[1])), (4, Row::At(at[0]))]
            }),
            &[3, 4],
            "3=30 4=42 9=90",
        ),
    ]);
}

// An entry whose generation is not its header's leads to another object's header:
// the entry `3 1` keeps the object `3 0 obj` heads, and `3 0 R` reads null, though
// the file holds that object. `PyMuPDF` 1.28.2 does not repair, and reads object 3
// as `30`.
#[test]
fn an_entry_whose_generation_is_not_its_headers_takes_the_object() {
    let file = file(&[(3, "30")], |at| vec![(3, Row::Gen(at[0], 1))]);
    let pdf = open("generation", file);
    assert_eq!(values(&pdf, &[3, 9]), "3=- 9=90");
    assert_eq!(value_of(&pdf, ObjectIdentifier::new(3, 1)), "30");
}

// An object is the one its entry names, whatever its header: where entries lead to
// another object's header, a page reads the object the entry leads to. `PyMuPDF`
// 1.28.2, once it repairs: in the first file the catalog is its later definition,
// `90`, and the file has no pages; in the second it sizes the page 612 × 792, having
// read entry 5 as null while it loaded the page; in the third it reads the page as
// here.
#[test]
fn entries_that_lead_to_other_objects_headers_read_those_objects() {
    // 4 leads to a second definition of the catalog: it is the page's /Rotate, and
    // the catalog is the first.
    let file = listed_at(
        &[
            (1, CATALOG),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            (3, "<< /Type /Page /MediaBox [0 0 300 300] /Rotate 4 0 R >>"),
            (1, "90"),
        ],
        &[(4, 3)],
        &[],
    );
    assert_eq!(
        inherited_in("numbered as the catalog", file),
        [(300.0, 1, vec![])]
    );

    // 5 leads to 4, which the table lists there too: both are 4's object.
    let file = listed_at(
        &[
            (1, CATALOG),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            (3, "<< /Type /Page /MediaBox 4 0 R /CropBox 5 0 R >>"),
            (4, "[0 0 300 300]"),
        ],
        &[(5, 3)],
        &[],
    );
    let pdf = open("two entries at one object", file);
    let page = &pdf.pages()[0];
    assert_eq!((page.media_box().x1, page.crop_box().x1), (300.0, 300.0));

    // Where the file holds the entry's own object, the entry is that object: page
    // 3's box is object 6, not the second object 7 that entry 6 leads to.
    let file = listed_at(
        &[
            (1, CATALOG),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            (3, "<< /Type /Page /Parent 2 0 R /MediaBox 6 0 R >>"),
            (7, "[0 0 200 300]"),
            (7, "[0 0 400 500]"),
            (6, "[0 0 600 700]"),
        ],
        &[(6, 4)],
        &[],
    );
    let pdf = open("the scan finds the entry's own", file);
    assert_eq!(pdf.pages()[0].media_box().x1, 600.0);
}

const DRAWING: &str = "0 0 1 rg 10 10 80 80 re f";

/// A page: its object number, its media box and its content.
type Drawn = (i32, [f64; 4], Vec<u8>);

/// Each page of `file`, as [`Drawn`] shows it.
fn drawn(name: &str, file: Vec<u8>) -> Vec<Drawn> {
    let pdf = open(name, file);
    pdf.pages()
        .iter()
        .map(|page| {
            let number = page.obj_id().map_or(0, |id| id.obj_number);
            let m = page.media_box();
            let content = page.try_page_stream().unwrap();
            (number, [m.x0, m.y0, m.x1, m.y1], content)
        })
        .collect()
}

// An object is read under the number of its entry: node 2 and its /Type, object 3,
// are both headed `1 0 obj`, and reading /Type from node 2 is no cycle, so node 2 is
// a node and page 4 the page. `PyMuPDF` 1.28.2 repairs, takes the last `1 0 obj`
// for the catalog, and finds no pages, here and in the next test.
#[test]
fn an_entry_read_from_a_node_headed_as_another_object_is_no_cycle() {
    let drawing = stream(DRAWING);
    let file = listed_at(
        &[
            (1, CATALOG),
            (
                1,
                "<< /Type 3 0 R /Kids [4 0 R] /Count 1 /MediaBox [0 0 300 300] >>",
            ),
            (1, "/Pages"),
            (
                4,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 5 0 R >>",
            ),
            (5, &drawing),
        ],
        &[(2, 1), (3, 2)],
        &[],
    );
    assert_eq!(
        drawn("node headed as another", file),
        [(4, [0.0, 0.0, 100.0, 100.0], DRAWING.as_bytes().to_vec())]
    );
}

// So too for /Kids: node 2's /Kids, object 3, lists page 5 before page 4, and both
// are headed `1 0 obj`.
#[test]
fn kids_read_from_a_node_headed_as_another_object_keep_their_order() {
    let page = |size: &str| {
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {size}] /Contents 6 0 R >>")
    };
    let drawing = stream(DRAWING);
    let file = listed_at(
        &[
            (1, CATALOG),
            (1, "<< /Type /Pages /Kids 3 0 R /Count 2 >>"),
            (1, "[5 0 R 4 0 R]"),
            (4, &page("100 200")),
            (5, &page("300 400")),
            (6, &drawing),
        ],
        &[(2, 1), (3, 2)],
        &[],
    );
    assert_eq!(
        drawn("kids headed as another", file),
        [
            (5, [0.0, 0.0, 300.0, 400.0], DRAWING.as_bytes().to_vec()),
            (4, [0.0, 0.0, 100.0, 200.0], DRAWING.as_bytes().to_vec()),
        ]
    );
}

// A table whose every offset leads a few bytes into its object's header, as in a
// file cut at its start, leads to a shorter header of another number over the
// object's own body (`2 0 obj` in `102 0 obj`): the rebuilt table has each object at
// its header, read whole. `PyMuPDF` reads the 37 pages of upstream's render test
// issue51 so, after its repair.
#[test]
fn a_table_whose_offsets_lead_into_the_headers_is_rebuilt() {
    for first in [11, 101, 1001] {
        let digits = first.to_string().len();
        let objects = [
            format!("<< /Type /Catalog /Pages {} 0 R >>", first + 1),
            format!("<< /Type /Pages /Kids [{} 0 R] /Count 1 >>", first + 2),
            format!(
                "<< /Type /Page /Parent {} 0 R /MediaBox [0 0 600 700] /Contents {} 0 R >>",
                first + 1,
                first + 3
            ),
            stream(DRAWING),
        ];
        for into in 1..digits {
            let mut d = Doc::new();
            let mut rows = vec![];
            for (i, object) in objects.iter().enumerate() {
                let number = first + i as i32;
                rows.push((number, Row::At(d.obj(number, object) + into)));
            }
            let table = d.table(&rows, Size::Auto, &format!("/Root {first} 0 R"));
            d.startxref(table);
            let name = format!("offsets {into} into headers from {first}");
            let page = (
                first + 2,
                [0.0, 0.0, 600.0, 700.0],
                DRAWING.as_bytes().to_vec(),
            );
            assert_eq!(drawn(&name, d.out), [page], "{name}");
        }
    }
}

/// Rows of the object streams 4 and, where `at` holds two, 6, at `at`; 5 as `five`;
/// and [`DAMAGED`].
fn in_streams(five: Option<Row>) -> impl Fn(&[usize]) -> Vec<(i32, Row)> {
    move |at: &[usize]| {
        let mut rows = vec![(4, Row::At(at[0]))];
        if let Some(six) = at.get(1) {
            rows.push((6, Row::At(*six)));
        }
        rows.extend(five.map(|row| (5, row)));
        rows.push(DAMAGED);
        rows
    }
}

// Of an object's definitions in object streams and outside them, a rebuilt table
// takes: where the old table lists the object in an object stream that holds it, the
// last definition in that stream; else, where the scan finds a definition outside
// object streams, the later of those; else the last definition in an object stream,
// in the order of the file. Where no table can be read, none lists the object.
//
// `PyMuPDF` 1.28.2 takes the last definition in the file, in an object stream or
// not, wherever it repairs (`DAMAGED` repairs at open). Deliberately, a rebuilt
// table here keeps the stream the old table lists, so an update that moves an object
// into an object stream shows the update, and else a definition outside object
// streams, as a file whose table cannot be read may hold object streams of objects it
// does not use under the numbers of those it does. `PyMuPDF` differs where a case
// names its value.
#[test]
fn a_rebuilt_table_takes_the_object_stream_the_old_table_lists() {
    let direct = || (5, "51".to_owned());
    let in_four = || (4, object_stream(&[(5, "54")]));
    let in_six = || (6, object_stream(&[(5, "56")]));
    let five = &[5][..];
    let mut cases: Vec<(String, Vec<u8>, &str, &str)> = vec![];
    // A direct 5 before the stream 4 or after it, at index `direct_at` of the
    // objects; 5 listed in 4 or directly.
    for (order, objects, direct_at) in [
        ("direct first", vec![direct(), in_four()], 0),
        ("stream first", vec![in_four(), direct()], 1),
    ] {
        let rows = |listed_in_stream: bool, damaged: bool| {
            move |at: &[usize]| {
                let five = if listed_in_stream {
                    Row::In(4, 0)
                } else {
                    Row::At(at[direct_at])
                };
                let mut rows = vec![(4, Row::At(at[1 - direct_at])), (5, five)];
                if damaged {
                    rows.push(DAMAGED);
                }
                rows
            }
        };
        let (rebuilt_in_stream, rebuilt_direct, no_table) = if direct_at == 0 {
            ("", "54", "54")
        } else {
            ("51", "", "")
        };
        cases.extend([
            (
                format!("{order}, listed in 4"),
                streamed(&objects, rows(true, false), true),
                "5=54 9=-",
                "",
            ),
            (
                format!("{order}, listed in 4, rebuilt"),
                streamed(&objects, rows(true, true), true),
                "5=54 9=90",
                rebuilt_in_stream,
            ),
            (
                format!("{order}, listed directly"),
                streamed(&objects, rows(false, false), true),
                "5=51 9=-",
                "",
            ),
            (
                format!("{order}, listed directly, rebuilt"),
                streamed(&objects, rows(false, true), true),
                "5=51 9=90",
                rebuilt_direct,
            ),
            (
                format!("{order}, no table"),
                streamed(&objects, rows(true, false), false),
                "5=51 9=90",
                no_table,
            ),
        ]);
    }
    // Two object streams, 4 and 6, and maybe a direct 5 after both.
    for (listing, five_row, expected, alone, with_direct) in [
        ("listed in 4", Some(Row::In(4, 0)), "5=54 9=90", "56", "51"),
        ("listed in 6", Some(Row::In(6, 0)), "5=56 9=90", "", "51"),
        ("not listed", None, "5=56 9=90", "", ""),
    ] {
        cases.push((
            format!("two streams, {listing}"),
            streamed(&[in_four(), in_six()], in_streams(five_row), true),
            expected,
            alone,
        ));
        let expected = if listing == "not listed" {
            "5=51 9=90"
        } else {
            expected
        };
        cases.push((
            format!("two streams and a direct 5, {listing}"),
            streamed(&[in_four(), in_six(), direct()], in_streams(five_row), true),
            expected,
            with_direct,
        ));
    }
    // The table lists 5 in 4, which does not hold it.
    let holds_seven = || (4, object_stream(&[(7, "70")]));
    cases.extend([
        (
            "listed in a stream that does not hold it".to_owned(),
            streamed(
                &[holds_seven(), in_six()],
                in_streams(Some(Row::In(4, 0))),
                true,
            ),
            "5=56 9=90",
            "",
        ),
        (
            "listed in a stream that does not hold it, and a direct 5".to_owned(),
            streamed(
                &[holds_seven(), in_six(), direct()],
                in_streams(Some(Row::In(4, 0))),
                true,
            ),
            "5=51 9=90",
            "",
        ),
    ]);
    // The last field names what `PyMuPDF` reads where it differs.
    for (name, file, expected, _pymupdf) in cases {
        assert_eq!(seen(&name, file, five), expected, "{name}");
    }
}

// Two definitions of object stream 4, the earlier holding 5 and the later only 7:
// the rebuilt table takes the later for 4, and reads 5 from the earlier, the stream
// its definition is in. Where the old table lists 5 in 4, the stream it lists is the
// definition its entry for 4 leads to: the earlier holds 5, the later does not, so
// a direct 5 is taken instead. `PyMuPDF` 1.28.2 reads 5 from the definition of 4
// its table holds, the later: as null, or as the direct `51` where there is one.
#[test]
fn a_rebuilt_table_reads_an_object_from_the_stream_that_holds_it() {
    let earlier = || (4, object_stream(&[(5, "54")]));
    let later = || (4, object_stream(&[(7, "70")]));
    let direct = || (5, "51".to_owned());
    // The rows of 4 at the definition `four` (an index of the objects), 5 as `five`
    // when `Some`, and [`DAMAGED`].
    let rows = |four: usize, five: Option<Row>| {
        move |at: &[usize]| {
            let mut rows = vec![(4, Row::At(at[four]))];
            rows.extend(five.map(|row| (5, row)));
            rows.push(DAMAGED);
            rows
        }
    };
    let in_four = Some(Row::In(4, 0));
    let numbers = &[5, 7][..];
    for (name, file, expected) in [
        (
            "not listed",
            streamed(&[earlier(), later()], rows(1, None), true),
            "5=54 7=70 9=90",
        ),
        (
            "no table",
            streamed(&[earlier(), later()], rows(1, None), false),
            "5=54 7=70 9=90",
        ),
        (
            "listed in the earlier",
            streamed(&[earlier(), later()], rows(0, in_four), true),
            "5=54 7=70 9=90",
        ),
        (
            "listed in the later",
            streamed(&[earlier(), later()], rows(1, in_four), true),
            "5=54 7=70 9=90",
        ),
        (
            "listed in the earlier, and a direct 5",
            streamed(&[direct(), earlier(), later()], rows(1, in_four), true),
            "5=54 7=70 9=90",
        ),
        (
            "listed in the later, and a direct 5",
            streamed(&[direct(), earlier(), later()], rows(2, in_four), true),
            "5=51 7=70 9=90",
        ),
    ] {
        assert_eq!(seen(name, file, numbers), expected, "{name}");
    }
}

// An object stream that holds 5 twice: the table's entry reads the copy at its
// index; a rebuilt table that lists 5 in it reads the last copy. `PyMuPDF` 1.28.2
// reads the first copy in all four.
#[test]
fn a_rebuilt_table_takes_the_last_copy_in_the_listed_stream() {
    let objects = [(4, object_stream(&[(5, "52"), (5, "53")]))];
    for (index, damaged, expected) in [
        (0, false, "5=52 9=-"),
        (0, true, "5=53 9=90"),
        (1, false, "5=53 9=-"),
        (1, true, "5=53 9=90"),
    ] {
        let rows = move |at: &[usize]| {
            let mut rows = vec![(4, Row::At(at[0])), (5, Row::In(4, index))];
            if damaged {
                rows.push(DAMAGED);
            }
            rows
        };
        let name = format!("twice in one stream, index {index}, rebuilt {damaged}");
        let file = streamed(&objects, rows, true);
        assert_eq!(seen(&name, file, &[5]), expected, "{name}");
    }
}

// The table of this file lists object 2, the page tree, in object stream 10, which it
// lacks: the table is rebuilt, and finds the page tree.
#[test]
fn a_table_entry_in_a_missing_object_stream_rebuilds_the_table() {
    let data = std::fs::read("../hayro-tests/pdfs/custom/lopdf_issue_449_2.pdf").unwrap();
    let pdf = Pdf::new(data).unwrap();
    let ids: Vec<_> = pdf.pages().iter().map(|p| p.obj_id()).collect();
    assert_eq!(ids, [Some(ObjectIdentifier::new(3, 0))]);
}

/// Object 4, a stream whose data hold the header of 5 and `five` after it, where 5's
/// entry leads; with `into`, an entry 6 leads to the first `into` from that header
/// on; with `damaged`, the table also has [`DAMAGED`].
fn in_data(five: &str, into: Option<&str>, damaged: bool) -> Vec<u8> {
    let mut d = Doc::new();
    let catalog = d.obj(1, CATALOG);
    let tree = d.obj(2, NO_PAGES);
    let four = d.obj(4, &stream(&format!("5 0 obj\n{five}\nendobj\n")));
    d.obj(UNLISTED, "90");
    let header = d.find(four + 1, "5 0 obj");
    let mut rows = vec![
        (1, Row::At(catalog)),
        (2, Row::At(tree)),
        (4, Row::At(four)),
        (5, Row::At(header)),
    ];
    if let Some(into) = into {
        rows.push((6, Row::At(d.find(header, into))));
    }
    if damaged {
        rows.push(DAMAGED);
    }
    let table = d.table(&rows, Size::Auto, "");
    d.startxref(table);
    d.out
}

// An entry of the old table that leads to its own header, whose number the scan does
// not find, here in a stream's data the scan does not read, is kept where an object
// that can be read follows the header, and lost where none does. `PyMuPDF` 1.28.2
// reads 5 before it repairs (`55`, `(a x c)`), and as null after.
#[test]
fn an_entry_whose_object_the_scan_does_not_find_is_kept() {
    expect(vec![
        (
            "in stream data",
            in_data("55", None, false),
            &[5],
            "5=55 9=-",
        ),
        (
            "in stream data, rebuilt",
            in_data("55", None, true),
            &[5],
            "5=55 9=90",
        ),
        // Entry 6 leads into 5's string, so 5's object does not end before the next
        // offset: the rebuild reads it to its end, and 6, with no header, is dropped.
        (
            "in stream data, an entry into its body",
            in_data("(a x c)", Some("x"), false),
            &[5, 6],
            "5=(a x c) 6=- 9=90",
        ),
        // Entry 6 leads into 5's header, which its offset cuts.
        (
            "in stream data, an entry into its header",
            in_data("55", Some("obj"), false),
            &[5, 6],
            "5=! 6=- 9=90",
        ),
        // The header 5's entry leads to is in a string of a dictionary, 3, where the
        // scan does not look for one: 5 is kept, and its place ends the slot of 3,
        // which is lost. `PyMuPDF` reads 3 whole, and 5 as null once it repairs.
        (
            "in a dictionary's string, rebuilt",
            {
                let mut d = Doc::new();
                let catalog = d.obj(1, CATALOG);
                let tree = d.obj(2, NO_PAGES);
                let three = d.obj(3, "<< /T (5 0 obj (y) endobj) >>");
                let five = d.find(three + 1, "5 0 obj");
                d.obj(UNLISTED, "90");
                let rows = [
                    (1, Row::At(catalog)),
                    (2, Row::At(tree)),
                    (3, Row::At(three)),
                    (5, Row::At(five)),
                    DAMAGED,
                ];
                let table = d.table(&rows, Size::Auto, "");
                d.startxref(table);
                d.out
            },
            &[3, 5],
            "3=! 5=(y) 9=90",
        ),
        (
            "in stream data, no object after the header",
            in_data("<< /A ] >>", None, true),
            &[5],
            "5=! 9=90",
        ),
    ]);
}

// A header with no object that can be read after it, which the scan finds, or an
// entry of the old table leads to, is listed but lost, unless the scan finds another
// definition of its number, before or after it. `PyMuPDF` 1.28.2 reads `<< /A ] >>`
// as `<<>>`, which as the later definition is 7 where a readable one comes before.
#[test]
fn a_header_without_an_object_is_lost_unless_defined_again() {
    let broken = "<< /A ] >>";
    expect(vec![
        (
            "found",
            file(&[(7, broken)], |_| vec![DAMAGED]),
            &[7],
            "7=! 9=90",
        ),
        (
            "listed",
            file(&[(7, broken)], |at| vec![(7, Row::At(at[0])), DAMAGED]),
            &[7],
            "7=! 9=90",
        ),
        (
            "defined again",
            file(&[(7, broken), (7, "70")], |at| {
                vec![(7, Row::At(at[0])), DAMAGED]
            }),
            &[7],
            "7=70 9=90",
        ),
        (
            "defined before",
            file(&[(7, "70"), (7, broken)], |at| {
                vec![(7, Row::At(at[1])), DAMAGED]
            }),
            &[7],
            "7=70 9=90",
        ),
        // In a rebuilt table, an object whose body runs past the next offset is
        // still cut there, and lost. `PyMuPDF` reads 3 whole, and 4 as null.
        (
            "a body past the next offset",
            nested(true),
            &[3, 4],
            "3=! 4=(y) 9=90",
        ),
    ]);
}

// A free entry lists nothing, and replaces nothing an earlier section lists: an
// update that frees 3 leaves 3 as the first section lists it. An entry in use at
// offset 0 is free. `PyMuPDF` 1.28.2 reads the freed 3 as null.
#[test]
fn a_free_entry_lists_nothing_and_replaces_nothing() {
    let three = [(3, "30")];
    expect(vec![
        (
            "free",
            file(&three, |_| vec![(3, Row::Free)]),
            &[3],
            "3=- 9=-",
        ),
        (
            "in use at offset 0",
            file(&three, |_| vec![(3, Row::At(0))]),
            &[3],
            "3=- 9=-",
        ),
        (
            "freed by an update",
            update(&[(3, BASE), (4, BASE)], &|_| vec![(3, Row::Free)], None),
            &[3, 4],
            "3=30 4=40 9=-",
        ),
    ]);
}
