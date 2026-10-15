//! Reading a damaged file, whatever entries its table has and wherever they lead,
//! works on each of its bytes a bounded number of times, and copying a page of it
//! writes a bounded number of bytes for each: from opening it, through its pages and
//! their geometry, to writing them.

use hayro_write::hayro_syntax::Pdf;
use hayro_write::hayro_syntax::object::{MaybeRef, Name, Stream};
use hayro_write::hayro_syntax::work::{BODIES_READ, EXAMINED, TOLD};
use hayro_write::pdf_writer::Ref;
use hayro_write::{ChunkSettings, ExtractionQuery, extract, extract_pages_to_pdf};

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

/// A name of 16 bytes, the `i`th of a family whose first 8 bytes cancel the seed of
/// rustc-hash's byte hashing, so that all hash alike with it: with a hash of fixed
/// keys, each inserted into a map is compared with every one before it.
fn colliding(i: usize) -> Vec<u8> {
    let mut name = b"/".to_vec();
    name.extend(0x243f_6a88_85a3_08d3_u64.to_le_bytes());
    name.extend(format!("{i:08x}").bytes());
    name
}

/// A file of a catalog, a page tree, pages that each read `/UserUnit` from an object
/// of an object stream, damaged pieces between them, the object stream, whose header
/// puts its objects at offsets of its data, inside a long token too, and a table,
/// classic or a stream, with entries into the pieces and anywhere in the file, or,
/// for a quarter of the files, none; `startxref` leads to the table, or to none,
/// which rebuilds it. In most, the pages draw an image, which up to 300 entries lead
/// to, each named in their resources, whose dictionary may refer to another object;
/// a stream table has up to 100 entries leading to the first object of the object
/// stream, each named in the pages' resources too. In some, the table's /Prev names
/// sections nested in each other's trailer strings. In some, the catalog holds a
/// dictionary of up to 3000 names that hash alike with a hash of fixed keys
/// ([`colliding`]), and in some, the pages name their graphics states so.
fn file(random: &mut Random) -> Vec<u8> {
    let pages = 1 + random.below(4);
    let damaged = random.chance(75);
    // Entries leading to the image, numbered from 600, and to the object stream's
    // first object, from 900.
    let images = if random.chance(60) {
        1 + random.below(300)
    } else {
        0
    };
    let refers = random.chance(50);
    let states = random.below(100);
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
    let mut object = |out: &mut Vec<u8>, number: u32, body: &[u8]| {
        objects.push((number, out.len()));
        out.extend(format!("{number} 0 obj\n").bytes());
        out.extend(body);
        out.extend(b"\nendobj\n");
    };
    let mut catalog = b"<< /Type /Catalog /Pages 2 0 R ".to_vec();
    if random.chance(20) {
        catalog.extend(b"/Test << ");
        for i in 0..1 + random.below(3000) {
            catalog.extend(colliding(i));
            catalog.extend(b" 0 ");
        }
        catalog.extend(b">> ");
    }
    catalog.extend(b">>");
    object(&mut out, 1, &catalog);
    let kids: Vec<String> = (0..pages).map(|p| format!("{} 0 R", 10 + p)).collect();
    object(
        &mut out,
        2,
        format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} /MediaBox [0 0 600 700] >>",
            kids.join(" ")
        )
        .as_bytes(),
    );
    let names: String = (0..images)
        .map(|j| format!("/A{j} {} 0 R ", 600 + j))
        .collect();
    let collide = random.chance(30);
    let state_names: String = (0..states)
        .map(|j| match collide {
            // The bytes of a colliding name, each escaped.
            true => {
                colliding(j)[1..]
                    .iter()
                    .fold("/".to_owned(), |name, b| name + &format!("#{b:02x}"))
                    + &format!(" {} 0 R ", 900 + j)
            }
            false => format!("/G{j} {} 0 R ", 900 + j),
        })
        .collect();
    let content = if images > 0 {
        "q 9 0 0 9 0 0 cm /A0 Do Q"
    } else {
        "0 0 m 1 1 l S"
    };
    for (p, unit) in units.iter().enumerate() {
        object(
            &mut out,
            10 + p as u32,
            format!(
                "<< /Type /Page /Parent 2 0 R /CropBox [0 0 {} 500] /UserUnit {} 0 R \
                 /Resources << /XObject << {names}>> /ExtGState << {state_names}>> >> \
                 /Contents {} 0 R >>",
                100 + p,
                20 + unit,
                500 + p
            )
            .as_bytes(),
        );
        object(
            &mut out,
            500 + p as u32,
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            )
            .as_bytes(),
        );
    }
    // The image the entries numbered from 600 lead to, and its color space.
    let mut aliases: Vec<(u32, usize)> = vec![];
    if images > 0 {
        let len = 64 + random.below(4096);
        let space = if refers { "7 0 R" } else { "/DeviceGray" };
        let at = out.len();
        object(
            &mut out,
            6,
            format!(
                "<< /Type /XObject /Subtype /Image /Width {len} /Height 1 \
                 /BitsPerComponent 8 /ColorSpace {space} /Length {len} >>\n\
                 stream\n{}\nendstream",
                "g".repeat(len)
            )
            .as_bytes(),
        );
        object(&mut out, 7, "/DeviceGray".as_bytes());
        aliases.extend((0..images as u32).map(|j| (600 + j, at)));
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
    // Sections the table's /Prev names, nested in each other's trailer strings.
    let prev = random.chance(15).then(|| {
        let first = out.len();
        let sections = 1 + random.below(40);
        let catalog = objects[0].1;
        let head = |next: Option<usize>| {
            let prev = next.map_or(format!("/Last {:010}", 0), |at| format!("/Prev {at:010}"));
            format!(
                "xref\n0 2\n0000000000 65535 f \n{catalog:010} 00000 n \n\
                 trailer\n<< /Size 2 {prev} /S ("
            )
        };
        let unit = head(None).len();
        for i in 0..sections {
            out.extend(head((i + 1 < sections).then(|| first + (i + 1) * unit)).bytes());
        }
        out.extend(b"p".repeat(random.below(4096)));
        out.extend(b") >>".repeat(sections));
        out.push(b'\n');
        format!(" /Prev {first}")
    });
    let prev = prev.unwrap_or_default();
    let table = out.len();
    let size = 1000 + entries.len() as u32 + 1;
    if random.chance(50) {
        // A classic table, which puts the objects of the stream nowhere.
        let mut rows = vec![None; size as usize];
        for &(number, at) in objects.iter().chain(&entries).chain(&aliases) {
            rows[number as usize] = Some(at);
        }
        out.extend(format!("xref\n0 {size}\n").bytes());
        for row in rows {
            match row {
                Some(at) => out.extend(format!("{at:010} 00000 n \n").bytes()),
                None => out.extend(b"0000000000 65535 f \n"),
            }
        }
        out.extend(format!("trailer\n<< /Size {size} /Root 1 0 R{prev} >>\n").bytes());
    } else {
        // A stream table, which puts them in the stream.
        let mut rows = vec![(0_u8, 0_u32, 0_u16); size as usize + 1];
        for &(number, at) in objects.iter().chain(&entries).chain(&aliases) {
            rows[number as usize] = (1, at as u32, 0);
        }
        for (index, number) in numbers.iter().enumerate() {
            if (*number as usize) < rows.len() {
                rows[*number as usize] = (2, stream, index as u16);
            }
        }
        for j in 0..states {
            rows[900 + j] = (2, stream, 0);
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
                "{size} 0 obj\n<< /Type /XRef /Size {} /W [1 4 2] /Root 1 0 R{prev} /Length {} >>\nstream\n",
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

/// The work of opening `file`, listing its pages, reading each page's geometry, and
/// copying them all ([`counted`]), and the bytes of the copy.
fn worked(file: Vec<u8>) -> (usize, usize) {
    count();
    let mut written = 0;
    if let Ok(pdf) = Pdf::new(file) {
        let pages = pdf.pages();
        for page in pages.iter() {
            let _ = (page.media_box(), page.crop_box(), page.rotation());
            let _ = pdf.xref().watch(|| page.raw().get::<f32>(b"UserUnit"));
        }
        let queries: Vec<_> = (0..pages.len()).map(ExtractionQuery::new_page).collect();
        let mut next = Ref::new(1);
        if let Ok(copy) = extract(
            &pdf,
            Box::new(|| next.bump()),
            ChunkSettings::default(),
            &queries,
        ) {
            written = copy.chunk.len();
        }
    }
    (counted(), written)
}

// Entries a table puts anywhere, many inside one long token, comment, string or run
// of white space, a sound table or one that is rebuilt, in the file or in an object
// stream, many leading to one image or object, and sections nested in each other:
// reading the file, its pages, their geometry, and copying them, works on each byte
// of the file a bounded number of times, and the copy holds a bounded number of
// bytes for each. Each read that starts where an entry leads lexes its slot only, and
// a slot is read by the table's check, the rebuild's probes and the table read: a
// few times; each section of a chain is read on its own bytes; each body is copied
// once, however many entries lead to it, its stream data hashed and written once.
// So the copy, of each page once, holds each object it draws with once, its data
// as stored and its dictionary rewritten, and each page's content once: no more
// than a few bytes for each byte of the file, where, as here, nothing the pages draw
// with is compressed, no two pages are one page object, no content array names a
// stream twice, and no two streams drawn share data. Where they do, the copy holds
// what the file means, which grows faster than the file.
#[test]
fn reading_a_damaged_file_works_on_it_a_bounded_number_of_times() {
    let mut random = Random(0x11ea_f00d);
    let (mut worst, mut largest) = ((0.0_f64, 0), (0.0_f64, 0));
    for case in 0..300 {
        let file = file(&mut random);
        let len = file.len();
        let (work, written) = worked(file);
        let ratio = work as f64 / len as f64;
        if ratio > worst.0 {
            worst = (ratio, case);
        }
        let ratio = written as f64 / len as f64;
        if ratio > largest.0 {
            largest = (ratio, case);
        }
        assert!(
            work <= 16 * len && written <= 2 * len,
            "case {case}: {work} steps and {written} bytes written for {len} bytes of file"
        );
    }
    eprintln!(
        "most work for each byte of a file: {:.2} (case {}); most bytes written: {:.2} (case {})",
        worst.0, worst.1, largest.0, largest.1
    );
}

/// Starts counting the work done on this thread: the bytes lexed, worked on
/// otherwise (decoded, decrypted, searched, hashed, compared, written) and read of
/// objects' bodies.
fn count() {
    EXAMINED.set(0);
    TOLD.set(0);
    BODIES_READ.set(0);
}

/// The work counted since [`count`].
fn counted() -> usize {
    EXAMINED.get() + TOLD.get() + BODIES_READ.get()
}

/// A file of `objects`, each its number and body, and a classic table listing them,
/// with `aliases` more entries, numbered from 10, at object 5.
fn listed(objects: &[(usize, String)], aliases: usize) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut rows = vec![None; 10 + aliases];
    for (number, body) in objects {
        rows[*number] = Some(out.len());
        out.extend(format!("{number} 0 obj\n{body}\nendobj\n").bytes());
    }
    for row in 10..10 + aliases {
        rows[row] = rows[5];
    }
    let table = out.len();
    out.extend(format!("xref\n0 {}\n", rows.len()).bytes());
    for row in &rows {
        match row {
            Some(at) => out.extend(format!("{at:010} 00000 n \n").bytes()),
            None => out.extend(b"0000000000 65535 f \n"),
        }
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{table}\n%%EOF\n",
            rows.len()
        )
        .bytes(),
    );
    out
}

/// A page drawing a gray image of `len` bytes, object 5, that `n` entries lead to,
/// numbered from 10, each named in the page's `/XObject` resources, `/A0` to `/A{n - 1}`;
/// the image's `/ColorSpace` is `/DeviceGray`, or, where `refers`, object 6, which is.
fn aliased_image(n: usize, len: usize, refers: bool) -> Vec<u8> {
    let names: String = (0..n).map(|i| format!("/A{i} {} 0 R ", 10 + i)).collect();
    let content = "q 600 0 0 700 0 0 cm /A0 Do Q";
    let space = if refers { "6 0 R" } else { "/DeviceGray" };
    listed(
        &[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".to_owned()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned()),
            (
                3,
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] \
                     /Resources << /XObject << {names}>> >> /Contents 4 0 R >>"
                ),
            ),
            (
                4,
                format!(
                    "<< /Length {} >>\nstream\n{content}\nendstream",
                    content.len()
                ),
            ),
            (
                5,
                format!(
                    "<< /Type /XObject /Subtype /Image /Width {len} /Height 1 \
                     /BitsPerComponent 8 /ColorSpace {space} /Length {len} >>\n\
                     stream\n{}\nendstream",
                    "g".repeat(len)
                ),
            ),
            (6, "/DeviceGray".to_owned()),
        ],
        n,
    )
}

/// The work of opening `file` and copying its first page, as a page and as a form,
/// and the bytes of the copy.
fn copied(file: Vec<u8>) -> (usize, usize) {
    count();
    let pdf = Pdf::new(file).unwrap();
    let queries = [
        ExtractionQuery::new_page(0),
        ExtractionQuery::new_user_space_xobject(0, [0.0, 0.0, 600.0, 700.0]),
    ];
    let mut next = Ref::new(1);
    let copy = extract(
        &pdf,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        &queries,
    )
    .unwrap();
    assert!(copy.root_refs.iter().all(Result::is_ok));
    (counted(), copy.chunk.len())
}

// Entries that lead to one image, each named in a page's resources, whether or not
// the image's dictionary refers to another object: copying the page reads, hashes
// and writes the image once, and every name draws the one copy, whose /ColorSpace is
// what the source's is. The work and the copy double with the file.
#[test]
fn entries_leading_to_one_image_copy_it_once() {
    for refers in [false, true] {
        let [small, large] = [128, 256].map(|n| {
            let len = 256 * n;
            let file = aliased_image(n, len, refers);
            let size = file.len();
            let source = Pdf::new(file.clone()).unwrap();
            let copy = Pdf::new(extract_pages_to_pdf(&source, &[0])).unwrap();
            let pages = copy.pages();
            let named: Vec<_> = pages[0]
                .resources()
                .x_objects
                .entries()
                .map(|(_, image)| match image {
                    MaybeRef::Ref(image) => image,
                    MaybeRef::NotRef(_) => panic!("an image copied into the resources"),
                })
                .collect();
            assert_eq!(named.len(), n, "refers {refers}");
            assert!(
                named.iter().all(|image| *image == named[0]),
                "refers {refers}"
            );
            let image = copy.xref().get::<Stream<'_>>(named[0].into()).unwrap();
            assert_eq!(image.raw_data().len(), len);
            assert_eq!(
                image.dict().get::<Name<'_>>(b"ColorSpace").as_deref(),
                Some(&b"DeviceGray"[..])
            );
            let (work, written) = copied(file);
            assert!(
                work <= 8 * size && written <= 2 * size,
                "refers {refers}, {n} entries: {work} steps and {written} bytes written \
                 for {size} bytes"
            );
            eprintln!(
                "refers {refers}, {n} entries: {work} steps, {written} bytes written for {size}"
            );
            (work, written)
        });
        assert!(
            large.0 <= small.0 * 25 / 10 && large.1 <= small.1 * 25 / 10,
            "refers {refers}: {small:?} for 128 entries, {large:?} for 256"
        );
    }
}
