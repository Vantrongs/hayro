use hayro_interpret::hayro_syntax::Pdf;
use std::fmt::Write;

pub(crate) fn document(page: &str, mut objects: Vec<String>) -> Pdf {
    let mut pdf = String::from("%PDF-1.7\n");
    let mut offsets = Vec::new();
    let tree = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_owned(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 128 96] /Contents 4 0 R {page} >>"),
    ];
    for (i, object) in tree.into_iter().chain(objects.drain(..)).enumerate() {
        offsets.push(pdf.len());
        writeln!(pdf, "{} 0 obj\n{object}\nendobj", i + 1).unwrap();
    }
    let xref = pdf.len();
    let size = offsets.len() + 1;
    writeln!(pdf, "xref\n0 {size}\n0000000000 65535 f ").unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        pdf,
        "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
    )
    .unwrap();
    Pdf::new(pdf.into_bytes()).unwrap()
}
