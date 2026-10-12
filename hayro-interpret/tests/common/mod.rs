//! Writes small PDF files and interprets their first page.

// Each test crate uses a different part of this module.
#![allow(dead_code)]

use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{Context, Device, InterpreterCache, InterpreterSettings, interpret_page};
use kurbo::{Affine, Rect};

/// A stream object with `dict` entries besides `/Length`.
pub(crate) fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut s = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
    s.extend_from_slice(data);
    s.extend_from_slice(b"\nendstream");
    s
}

/// A PDF file whose objects are numbered from 1 in this order; object 1 is the catalog.
pub(crate) fn file(objects: &[Vec<u8>]) -> Vec<u8> {
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
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

/// A file with one `size` × `size` pt page whose dictionary has `entries` besides its
/// type, parent and media box; `objects` are numbered from 4.
pub(crate) fn one_page(size: u32, entries: &str, objects: Vec<Vec<u8>>) -> Vec<u8> {
    let mut all = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {size} {size}] {entries} >>")
            .into_bytes(),
    ];
    all.extend(objects);
    file(&all)
}

/// Interprets the first page of `pdf` into `device` in PDF user space (y up), so
/// coordinates read like the numbers in the file.
pub(crate) fn interpret_first_page<'a>(
    pdf: &'a Pdf,
    settings: InterpreterSettings,
    device: &mut impl Device<'a>,
) {
    let page = &pdf.pages()[0];
    let (w, h) = page.render_dimensions();
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(
        Affine::IDENTITY,
        Rect::new(0.0, 0.0, f64::from(w), f64::from(h)),
        &cache,
        page.xref(),
        settings,
    );
    interpret_page(page, &mut ctx, device);
}
