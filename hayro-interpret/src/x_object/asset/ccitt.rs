//! CCITT fax images, read as rows of the samples whole decoding makes of them
//! ([`Streamed`](super::streamed::Streamed)).

#[cfg(test)]
mod tests {
    use super::super::Plane;
    use super::super::differential::assert_streams_as_whole;
    use super::super::tests::{
        assert_asset_streams_as_decoded, asset, image, is_streamed, named, pdf, request, transfer,
        whole,
    };
    use std::sync::Arc;

    /// A 211 × 96 test page in each encoding, as `hayro-ccitt`'s tests have it.
    const G4: &[u8] = include_bytes!("../../../../hayro-ccitt/tests/streams/g4.bin");
    const G3_1D: &[u8] = include_bytes!("../../../../hayro-ccitt/tests/streams/g3_1d.bin");
    const G3_2D: &[u8] = include_bytes!("../../../../hayro-ccitt/tests/streams/g3_2d.bin");
    /// The G4 page ended by an end-of-block code.
    const G4_EOFB: &[u8] = include_bytes!("../../../../hayro-ccitt/tests/streams/g4_eofb.bin");

    fn ccitt(k: i32, extra: &str) -> String {
        format!("/Filter /CCITTFaxDecode /DecodeParms << /K {k} /Columns 211 {extra} >>")
    }

    /// Images and stencil masks of CCITT data stream as they decode whole, whatever
    /// the encoding, `BlackIs1` and `/Decode`, under a transfer function, and with a
    /// `/Width` other than the data's columns.
    #[test]
    fn ccitt_images_stream_as_decoded() {
        for (k, data) in [(-1, G4), (0, G3_1D), (1, G3_2D)] {
            for kind in [
                "/ColorSpace /DeviceGray /BitsPerComponent 1",
                "/ColorSpace /DeviceGray /BitsPerComponent 1 /Decode [1 0]",
                "/ImageMask true",
                "/ImageMask true /Decode [1 0]",
            ] {
                for params in ["", "/BlackIs1 true"] {
                    for width in [211, 100] {
                        let dict = format!("{kind} {}", ccitt(k, params));
                        let pdf = pdf(&[(image(&dict, width, 96), data.to_vec())]);
                        let name = format!("K {k}, {kind}, {params}, width {width}");
                        let check = |four: Option<bool>| {
                            let mut asset = asset(&pdf);
                            if let Some(four) = four {
                                Arc::get_mut(&mut asset.0).unwrap().transfer_function =
                                    Some(transfer(four));
                            }
                            named(&format!("{name}, transfer {four:?}"), || {
                                assert_asset_streams_as_decoded(&asset);
                            });
                        };
                        check(None);
                        if !kind.contains("ImageMask") {
                            check(Some(false));
                            check(Some(true));
                        }
                    }
                }
            }
        }
    }

    /// Data that ends before the image's rows, breaks off within them or holds none
    /// of them: the image keeps its declared rows, and those the data does not give
    /// are black, whatever `BlackIs1` says, as `MuPDF` draws them.
    #[test]
    fn ccitt_data_that_ends_early_streams_as_decoded() {
        let gray = "/ColorSpace /DeviceGray /BitsPerComponent 1";
        for (height, data) in [
            (150, G4_EOFB.to_vec()),
            (5_000, G4_EOFB.to_vec()),
            // Without an end-of-block code: the data fails past its rows.
            (150, G4.to_vec()),
            (96, G4[..G4.len() / 2].to_vec()),
            (96, vec![0xFF; 4]),
            (96, vec![0; 4]),
            (96, vec![0x12, 0x34, 0x56, 0x78]),
            (96, vec![]),
        ] {
            for params in ["", "/BlackIs1 true"] {
                let case = format!("height {height}, {} bytes, {params}", data.len());
                let pdf = pdf(&[(
                    image(&format!("{gray} {}", ccitt(-1, params)), 211, height),
                    data.clone(),
                )]);
                let asset = asset(&pdf);
                assert_streams_as_whole(&asset, &case);
                let mut whole = whole(&asset);
                assert_eq!(whole.layout().image.height, height, "{case}");
                let last = request(
                    &mut whole,
                    Plane::Image,
                    (211, height),
                    [0, height - 1, 211, height],
                );
                assert!(
                    last.data.iter().all(|&v| v == 0),
                    "{case}: the last row is black"
                );
            }
        }
    }

    /// Opening reads none of the data, and a request decodes the rows through the
    /// last one it reads and no further.
    #[test]
    fn a_ccitt_request_decodes_through_its_rows() {
        let gray = "/ColorSpace /DeviceGray /BitsPerComponent 1";
        let pdf = pdf(&[(
            image(&format!("{gray} {}", ccitt(-1, "")), 211, 96),
            G4.to_vec(),
        )]);
        let asset = asset(&pdf);
        let polls = std::cell::Cell::new(0);
        let cancel = || {
            polls.set(polls.get() + 1);
            false
        };
        let mut source = asset.open(None, Some(&cancel)).unwrap();
        assert_eq!(polls.get(), 1, "opening polled once, as it starts");
        assert!(is_streamed(&source));
        let a = request(&mut source, Plane::Image, (211, 96), [10, 20, 30, 40]);
        assert_eq!(source.work().rows_decoded, 40);
        let b = request(
            &mut whole(&asset),
            Plane::Image,
            (211, 96),
            [10, 20, 30, 40],
        );
        assert_eq!(a.data, b.data);
    }

    /// A request of the first row of data that breaks off later returns it, as whole
    /// decoding does, and the rows past the break are black.
    #[test]
    fn a_request_before_the_data_breaks_returns_its_rows() {
        let gray = "/ColorSpace /DeviceGray /BitsPerComponent 1";
        let pdf = pdf(&[(
            image(&format!("{gray} {}", ccitt(-1, "")), 211, 96),
            G4[..G4.len() / 2].to_vec(),
        )]);
        let asset = asset(&pdf);
        let mut source = asset.open(None, None).unwrap();
        let a = request(&mut source, Plane::Image, (211, 96), [0, 0, 211, 1]);
        assert_eq!(source.work().rows_decoded, 1);
        let mut whole = whole(&asset);
        assert_eq!(whole.layout().image.height, 96);
        let b = request(&mut whole, Plane::Image, (211, 96), [0, 0, 211, 1]);
        assert_eq!(a.data, b.data);
        let last = request(&mut source, Plane::Image, (211, 96), [0, 95, 211, 96]);
        assert!(last.data.iter().all(|&v| v == 0));
    }
}
