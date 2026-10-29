//! Writes small PDF files and interprets their first page.

// Each test crate uses a different part of this module.
#![allow(dead_code, unused_imports)]

use hayro_interpret::annotation::View;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{Context, Device, InterpreterCache, InterpreterSettings, interpret_page};
use kurbo::{Affine, Rect};

#[path = "../../../hayro-syntax/tests/common/fixtures.rs"]
mod fixtures;
pub(crate) use fixtures::{
    file, file_with_aliases, noise, one_page, one_page_objects, stored_zlib, stream,
};

#[path = "../../../hayro-syntax/tests/common/chains.rs"]
pub(crate) mod chains;

#[path = "../../../hayro-syntax/tests/common/cpu_time.rs"]
mod cpu_time;
pub(crate) use cpu_time::thread_cpu_time;

/// Interprets the first page of `pdf` into `device` in PDF user space (y up), so
/// coordinates read like the numbers in the file.
pub(crate) fn interpret_first_page<'a>(
    pdf: &'a Pdf,
    settings: InterpreterSettings,
    device: &mut impl Device<'a>,
) {
    let view = View::of(&pdf.pages()[0]);
    interpret_first_page_as(pdf, settings, &view, device);
}

/// [`interpret_first_page`], with annotations as they look on the page shown as
/// `view` says.
pub(crate) fn interpret_first_page_as<'a>(
    pdf: &'a Pdf,
    settings: InterpreterSettings,
    view: &View,
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
    interpret_page(page, view, &mut ctx, device);
}
