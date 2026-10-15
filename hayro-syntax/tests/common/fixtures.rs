//! Small PDF files and the data in them, for tests of this crate and of hayro-interpret.

/// A stream object with `dict` entries besides `/Length`.
pub(crate) fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut s = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    s.extend_from_slice(data);
    s.extend_from_slice(b"\nendstream");
    s
}

/// A PDF file whose objects are numbered from 1 in this order, object 1 being the
/// catalog, with `trailer` entries besides `/Size` and `/Root`.
pub(crate) fn file(objects: &[Vec<u8>], trailer: &str) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    let size = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R {trailer} >>\nstartxref\n{xref}\n%%EOF\n")
            .as_bytes(),
    );
    out
}

/// A file of `objects` numbered from 1, as [`file`] writes them, whose table
/// also lists objects from `objects.len() + 1` on, one for each of `aliases`, at the
/// place of object `aliases[i]`. Those entries do not lead to their objects, so the
/// file is read by a scan, which finds no object of theirs and keeps them: each
/// leads to the body of the object whose place it names.
pub(crate) fn file_with_aliases(objects: Vec<Vec<u8>>, aliases: &[usize]) -> Vec<u8> {
    let size = objects.len() + aliases.len() + 1;
    let held: usize = objects.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(held + 32 * objects.len() + 20 * size + 128);
    out.extend_from_slice(b"%PDF-1.7\n");
    let mut offsets = Vec::new();
    for (i, o) in objects.into_iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(&o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let aliased: Vec<usize> = aliases.iter().map(|a| offsets[a - 1]).collect();
    offsets.extend(aliased);
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

/// The catalog, page tree and dictionary of one `size` × `size` pt page with `entries`
/// besides its type, parent and media box: objects 1 to 3.
pub(crate) fn one_page_objects(size: u32, entries: &str) -> Vec<Vec<u8>> {
    vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {size} {size}] {entries} >>")
            .into_bytes(),
    ]
}

/// A file with one page (see `one_page_objects`) and `objects` numbered from 4.
pub(crate) fn one_page(size: u32, entries: &str, objects: Vec<Vec<u8>>) -> Vec<u8> {
    let mut all = one_page_objects(size, entries);
    all.extend(objects);
    file(&all, "")
}

/// `data` as raw deflate of stored (uncompressed) blocks of at most `block` bytes,
/// which every Flate decoder reads without a compressor.
pub(crate) fn stored_deflate(data: &[u8], block: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut blocks = data.chunks(block).peekable();
    if blocks.peek().is_none() {
        out.extend([1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(b) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let len = b.len() as u16;
        out.extend(len.to_le_bytes());
        out.extend((!len).to_le_bytes());
        out.extend_from_slice(b);
    }
    out
}

/// `stored_deflate` in a zlib stream, with its header and checksum.
pub(crate) fn stored_zlib(data: &[u8], block: usize) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend(stored_deflate(data, block));
    let (mut a, mut b) = (1_u32, 0_u32);
    for &d in data {
        a = (a + u32::from(d)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend(((b << 16) | a).to_be_bytes());
    out
}

/// `n` deterministic pseudo-random bytes.
pub(crate) fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}
