//! Entries of an encrypted file's table that lead to one object are each that object,
//! read with its own key: pages drawn with them keep their own colors.

use hayro_syntax::Pdf;
use hayro_syntax::object::{Array, Name, Object, ObjectIdentifier};

mod common;
use common::rc4::{self, PALETTE, aliased};

/// The palette of color space /CS of `page`'s resources.
fn palette(pdf: &Pdf, page: usize) -> Vec<u8> {
    let space = pdf.pages()[page]
        .resources()
        .get_color_space(&Name::new(b"CS").unwrap())
        .and_then(Object::into_array)
        .expect("an indexed color space");
    palette_of(&space)
}

fn palette_of(space: &Array<'_>) -> Vec<u8> {
    match space.iter::<Object<'_>>().nth(3) {
        Some(Object::String(palette)) => palette.to_vec(),
        other => panic!("no palette: {other:?}"),
    }
}

// Page 2 reads its palette as the entry it draws with decrypts it, whether the
// entry is its resources or their color spaces: the same bytes under page 1's
// entry are another palette, which loading the pages does not hand to page 2.
#[test]
fn pages_drawn_with_entries_leading_to_one_object_keep_their_colors() {
    for (whole, alias, target) in [(true, 9, 6), (false, 8, 7)] {
        let pdf = Pdf::new(aliased(whole)).unwrap();
        assert_eq!(palette(&pdf, 0), PALETTE, "whole: {whole}");
        let own = rc4::crypt(alias, &rc4::crypt(target, PALETTE));
        assert_ne!(own, PALETTE);
        assert_eq!(palette(&pdf, 1), own, "whole: {whole}");
        // As the entry read alone holds it.
        let object = pdf
            .xref()
            .get::<Object<'_>>(ObjectIdentifier::new(alias as i32, 0))
            .unwrap();
        let space = match object {
            Object::Dict(dict) if whole => dict
                .get::<hayro_syntax::object::Dict<'_>>(b"ColorSpace")
                .unwrap()
                .get::<Array<'_>>(b"CS")
                .unwrap(),
            Object::Dict(dict) => dict.get::<Array<'_>>(b"CS").unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(palette_of(&space), own, "whole: {whole}");
    }
}
