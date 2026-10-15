//! Reading a damaged file, whatever entries its table has and wherever they lead,
//! lexes it a bounded number of times: from opening it, through its pages and
//! their geometry, to writing them.

use hayro_write::hayro_syntax::Pdf;
use hayro_write::hayro_syntax::work::EXAMINED;
use hayro_write::pdf_writer::Ref;
use hayro_write::{ChunkSettings, ExtractionQuery, extract};

/// A small deterministic generator.
struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n.max(1) as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

/// A run of bytes a damaged file holds between its objects, where many entries may
/// lead, and how far apart they may be: inside one token, comment, string or run of
/// white space, or among headers and objects.
fn piece(random: &mut Random) -> Vec<u8> {
    let long = 64 + random.below(3000);
    match random.below(12) {
        // Digits of one number, with zeros before a header's number.
        0 => "1".repeat(long).into_bytes(),
        1 => format!("{}9 0 obj\nnull\nendobj\n", "0".repeat(long)).into_bytes(),
        // White space, and white space after a header.
        2 => " ".repeat(long).into_bytes(),
        3 => format!("7 0 obj\n{}<< /A 1 >>\nendobj\n", "\n".repeat(long)).into_bytes(),
        // A comment line holding headers, before white space.
        4 => format!(
            "%{}\n{}",
            " 8 0 obj".repeat(long / 8),
            " ".repeat(random.below(long))
        )
        .into_bytes(),
        // A string, ended or not, holding headers.
        5 => format!(
            "({}{}",
            "6 0 obj ".repeat(long / 8),
            if random.chance(50) { ")" } else { "" }
        )
        .into_bytes(),
        // A name, and a keyword.
        6 => format!("/{}", "a".repeat(long)).into_bytes(),
        7 => "x".repeat(long).into_bytes(),
        // Containers, nested, ended or not.
        8 => format!(
            "{}{}",
            "[ << /K ".repeat(long / 8),
            if random.chance(50) {
                "1 >> ] ".repeat(long / 8)
            } else {
                String::new()
            }
        )
        .into_bytes(),
        // Streams, one after another, the data of each reaching over the next.
        9 => {
            let mut out = vec![];
            for _ in 0..1 + random.below(8) {
                out.extend(
                    format!(
                        "5 0 obj\n<< /Length {} >>\nstream\n{}",
                        random.below(long),
                        "d".repeat(random.below(64))
                    )
                    .bytes(),
                );
            }
            out.extend(b"\nendstream\nendobj\n");
            out
        }
        // Headers split by comments, and dictionaries each before a comment.
        10 => "9 %c\n0 obj ".repeat(long / 10).into_bytes(),
        _ => "<< /A 1 >> % c\n".repeat(long / 16).into_bytes(),
    }
}

/// Where an entry leads in `piece`, which starts at `base`: `count` offsets one
/// after another, `step` apart, from a random one.
fn offsets(random: &mut Random, base: usize, piece: usize, count: usize) -> Vec<usize> {
    let step = 1 + random.below(16);
    let from = random.below(piece);
    (0..count)
        .map(|i| from + i * step)
        .filter(|at| *at < piece)
        .map(|at| base + at)
        .collect()
}

/// A file of a catalog, a page tree, pages that each read `/UserUnit` from an object
/// of an object stream, damaged pieces between them, the object stream, whose header
/// puts its objects at offsets of its data, inside a long token too, and a table,
/// classic or a stream, with entries into the pieces and anywhere in the file, or,
/// for a quarter of the files, none; `startxref` leads to the table, or to none,
/// which rebuilds it.
fn file(random: &mut Random) -> Vec<u8> {
    let pages = 1 + random.below(4);
    let damaged = random.chance(75);
    // The object stream's objects each page reads, of `pages` and up to 200 more.
    let units: Vec<usize> = (0..pages)
        .map(|p| {
            if random.chance(50) {
                p
            } else {
                random.below(pages + 200)
            }
        })
        .collect();
    let mut out = b"%PDF-1.7\n".to_vec();
    // Each object of the file, by number, at its offset.
    let mut objects: Vec<(u32, usize)> = vec![];
    let mut object = |out: &mut Vec<u8>, number: u32, body: &str| {
        objects.push((number, out.len()));
        out.extend(format!("{number} 0 obj\n{body}\nendobj\n").bytes());
    };
    object(&mut out, 1, "<< /Type /Catalog /Pages 2 0 R >>");
    let kids: Vec<String> = (0..pages).map(|p| format!("{} 0 R", 10 + p)).collect();
    object(
        &mut out,
        2,
        &format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} /MediaBox [0 0 600 700] >>",
            kids.join(" ")
        ),
    );
    for (p, unit) in units.iter().enumerate() {
        object(
            &mut out,
            10 + p as u32,
            &format!(
                "<< /Type /Page /Parent 2 0 R /CropBox [0 0 {} 500] /UserUnit {} 0 R \
                 /Contents {} 0 R >>",
                100 + p,
                20 + unit,
                500 + p
            ),
        );
        object(
            &mut out,
            500 + p as u32,
            "<< /Length 13 >>\nstream\n0 0 m 1 1 l S\nendstream",
        );
    }
    // The damaged pieces, and where entries lead into them.
    let mut inside = vec![];
    for _ in 0..1 + random.below(6) {
        let base = out.len();
        let piece = piece(random);
        let count = if damaged { random.below(800) } else { 0 };
        inside.extend(offsets(random, base, piece.len(), count));
        out.extend(piece);
        out.push(b'\n');
    }
    // The object stream, holding each page's unit, at offsets of its data that may
    // be inside a long token.
    let data = {
        let mut data = vec![];
        let mut starts = vec![];
        for _ in 0..pages {
            starts.push(data.len());
            data.extend(format!("{} ", 1 + random.below(3)).bytes());
        }
        let pad = piece(random);
        let at = data.len();
        let count = random.below(200);
        let extra = offsets(random, 0, pad.len(), count);
        data.extend(pad);
        starts.extend(extra.into_iter().map(|offset| at + offset));
        (starts, data)
    };
    let (starts, data) = data;
    let numbers: Vec<u32> = (0..starts.len() as u32).map(|i| 20 + i).collect();
    let header: String = numbers
        .iter()
        .zip(&starts)
        .map(|(number, at)| format!("{number} {at} "))
        .collect();
    let stream = 3;
    objects.push((stream, out.len()));
    out.extend(
        format!(
            "{stream} 0 obj\n<< /Type /ObjStm /N {} /First {} /Length {} >>\nstream\n{header}",
            numbers.len(),
            header.len(),
            header.len() + data.len()
        )
        .bytes(),
    );
    out.extend(&data);
    out.extend(b"\nendstream\nendobj\n");
    // Entries into the pieces and anywhere, numbered after every other.
    if damaged {
        for _ in 0..random.below(50) {
            inside.push(random.below(out.len()));
        }
    }
    let entries: Vec<(u32, usize)> = inside
        .iter()
        .enumerate()
        .map(|(i, at)| (1000 + i as u32, *at))
        .collect();
    let broken = damaged && random.chance(30);
    let table = out.len();
    let size = 1000 + entries.len() as u32 + 1;
    if random.chance(50) {
        // A classic table, which puts the objects of the stream nowhere.
        let mut rows = vec![None; size as usize];
        for &(number, at) in objects.iter().chain(&entries) {
            rows[number as usize] = Some(at);
        }
        out.extend(format!("xref\n0 {size}\n").bytes());
        for row in rows {
            match row {
                Some(at) => out.extend(format!("{at:010} 00000 n \n").bytes()),
                None => out.extend(b"0000000000 65535 f \n"),
            }
        }
        out.extend(format!("trailer\n<< /Size {size} /Root 1 0 R >>\n").bytes());
    } else {
        // A stream table, which puts them in the stream.
        let mut rows = vec![(0_u8, 0_u32, 0_u16); size as usize + 1];
        for &(number, at) in objects.iter().chain(&entries) {
            rows[number as usize] = (1, at as u32, 0);
        }
        for (index, number) in numbers.iter().enumerate() {
            if (*number as usize) < rows.len() {
                rows[*number as usize] = (2, stream, index as u16);
            }
        }
        rows[size as usize] = (1, table as u32, 0);
        let mut bytes = vec![];
        for (kind, field, index) in rows {
            bytes.push(kind);
            bytes.extend(field.to_be_bytes());
            bytes.extend(index.to_be_bytes());
        }
        out.extend(
            format!(
                "{size} 0 obj\n<< /Type /XRef /Size {} /W [1 4 2] /Root 1 0 R /Length {} >>\nstream\n",
                size + 1,
                bytes.len()
            )
            .bytes(),
        );
        out.extend(bytes);
        out.extend(b"\nendstream\nendobj\n");
    }
    let start = if broken { 9 } else { table };
    out.extend(format!("startxref\n{start}\n%%EOF\n").bytes());
    out
}

/// The bytes lexed to open `file`, list its pages, read each page's geometry, and
/// write them all.
fn lexed(file: Vec<u8>) -> usize {
    EXAMINED.set(0);
    if let Ok(pdf) = Pdf::new(file) {
        let pages = pdf.pages();
        for page in pages.iter() {
            let _ = (page.media_box(), page.crop_box(), page.rotation());
            let _ = pdf.xref().watch(|| page.raw().get::<f32>(b"UserUnit"));
        }
        let queries: Vec<_> = (0..pages.len()).map(ExtractionQuery::new_page).collect();
        let mut next = Ref::new(1);
        let _ = extract(
            &pdf,
            Box::new(|| next.bump()),
            ChunkSettings::default(),
            &queries,
        );
    }
    EXAMINED.get()
}

// Entries a table puts anywhere, many inside one long token, comment, string or run
// of white space, a sound table or one that is rebuilt, in the file or in an object
// stream: reading the file, its pages, their geometry, and writing them, lexes at
// most a bounded number of bytes for each byte of the file. Each read that starts
// where an entry leads lexes its slot only, and a slot is read by the table's check,
// the rebuild's probes and the table read: a few times.
#[test]
fn reading_a_damaged_file_lexes_it_a_bounded_number_of_times() {
    let mut random = Random(0x11ea_f00d);
    let mut worst = (0.0_f64, 0);
    for case in 0..300 {
        let file = file(&mut random);
        let len = file.len();
        let lexed = lexed(file);
        let ratio = lexed as f64 / len as f64;
        if ratio > worst.0 {
            worst = (ratio, case);
        }
        assert!(
            lexed <= 8 * len,
            "case {case}: {lexed} bytes lexed for {len} bytes of file"
        );
    }
    eprintln!(
        "most bytes lexed for each byte of a file: {:.2} (case {})",
        worst.0, worst.1
    );
}
