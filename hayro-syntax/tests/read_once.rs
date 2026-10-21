//! Bodies read once are not kept: a file that writes thousands of colour spaces
//! apart, each read once through a large dictionary of resources whose keys come out
//! of order, as `pdfbox` 5952 does, is read holding the room of the table of bodies
//! read and of the dictionary, not that of every colour space parsed. The bytes held
//! at most and the allocations stay under ceilings.

use hayro_syntax::Pdf;
use hayro_syntax::object::{Array, Dict, Name, ObjectIdentifier};

mod common;
use common::allocations::measure;
use common::ceilings::check_ceilings;
use common::file;

/// How many colour spaces the file writes.
const SPACES: usize = 4000;

/// A file whose object 3 lists colour spaces `/R0` to `/R3999` in that order, which
/// is not the order of their keys' bytes, each a `DeviceN` array of its own object
/// naming a colorant with escapes and two shared objects.
fn colour_spaces() -> Vec<u8> {
    let first = 6;
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [4 0 R] /Count 1 >>".to_vec(),
    ];
    let mut listed = String::from("<<");
    for i in 0..SPACES {
        listed.push_str(&format!(" /R{i} {} 0 R", first + i));
    }
    listed.push_str(" >>");
    objects.push(listed.into_bytes());
    objects.push(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] >>".to_vec());
    objects.push(b"<< /FunctionType 2 /Domain [0 1] /N 1 >>".to_vec());
    for _ in 0..SPACES {
        objects.push(
            b"[/DeviceN [/Cyan /Magenta /Yellow /Black /PANTONE#20Trans.#20White] \
              /DeviceRGB 5 0 R 1 0 R]"
                .to_vec(),
        );
    }
    file(&objects, "")
}

/// Every colour space read once through the dictionary listing them, each dropped
/// before the next is read, as a page that sets each once reads them, holds at most
/// the table of the bodies read and the dictionary, not the colour spaces parsed:
/// reading them while holding every one holds each parsed colour space on top.
#[test]
fn colour_spaces_read_once_are_not_kept() {
    let data = colour_spaces();
    // What a thread allocates once, whichever thread runs the test, is not counted.
    read_all(&Pdf::new(data.clone()).unwrap(), &mut None);
    let held_data = data.clone();
    let ((), held) = measure(move || {
        let pdf = Pdf::new(held_data).unwrap();
        read_all(&pdf, &mut Some(Vec::new()));
    });
    let ((), once) = measure(move || read_all(&Pdf::new(data).unwrap(), &mut None));
    // A colour space parsed takes 10 nodes of 32 bytes and a body.
    assert!(
        once.peak + SPACES * 10 * 32 < held.peak,
        "{} bytes held reading {SPACES} colour spaces once, {} holding them",
        once.peak,
        held.peak
    );
    check_ceilings(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ceilings/read_once.txt"),
        "WRITE_CEILINGS=1 cargo test -p hayro-syntax --test read_once",
        &[
            ("read_peak_bytes", once.peak),
            ("read_allocations", once.count),
        ],
    );
}

/// Reads every colour space, keeping each in `kept` when it is given.
fn read_all<'a>(pdf: &'a Pdf, kept: &mut Option<Vec<Array<'a>>>) {
    let spaces = pdf
        .xref()
        .get::<Dict<'_>>(ObjectIdentifier::new(3, 0))
        .unwrap();
    assert_eq!(spaces.len(), SPACES);
    for i in 0..SPACES {
        let space = spaces.get::<Array<'_>>(format!("R{i}").as_bytes()).unwrap();
        let colorants = space.iter::<Array<'_>>().next().unwrap();
        let last = colorants.iter::<Name<'_>>().last().unwrap();
        assert_eq!(last.as_ref(), b"PANTONE Trans. White");
        if let Some(kept) = kept {
            kept.push(space);
        }
    }
}
