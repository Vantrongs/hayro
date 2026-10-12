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
