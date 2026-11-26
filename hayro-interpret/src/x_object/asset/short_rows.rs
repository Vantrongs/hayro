use super::*;
use hayro_syntax::Pdf;
use hayro_syntax::object::{ObjectIdentifier, Stream};

pub(super) fn pdf(images: &[(&str, &[u8])]) -> Pdf {
    let mut file=b"%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [] /Count 0 >> endobj\n".to_vec();
    for (i, (dict, data)) in images.iter().enumerate() {
        let height = if dict.contains("/Height ") {
            ""
        } else {
            "/Height 3"
        };
        let width = if dict.contains("/Width ") {
            ""
        } else {
            "/Width 2"
        };
        file.extend(format!("{} 0 obj << /ColorSpace /DeviceGray {width} {height} {dict} /Length {} >>\nstream\n",3+i,data.len()).bytes());
        file.extend_from_slice(data);
        file.extend(b"\nendstream\nendobj\n");
    }
    file.extend(b"trailer << /Root 1 0 R >>\n%%EOF");
    Pdf::new(file).unwrap()
}

pub(super) fn image(pdf: &Pdf) -> ImageXObject<'_> {
    let stream = pdf
        .xref()
        .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
        .unwrap();
    ImageXObject::new(
        &stream,
        |_| None,
        &(Arc::new(|_| {}) as WarningSinkFn),
        &Cache::new(),
        None,
    )
    .unwrap()
}

fn replace_mask_on_repair(bytes: &mut [u8]) {
    for (header, replacement) in [(b"5 0 obj", b'9'), (b"6 0 obj", b'4')] {
        let offset = bytes
            .windows(header.len())
            .position(|w| w == header)
            .unwrap();
        if header == b"6 0 obj" {
            // Leave the replacement unclaimed so repair can index its physical header.
            let row = format!("{offset:010} 00000 n");
            let start = bytes
                .windows(row.len())
                .position(|w| w == row.as_bytes())
                .unwrap();
            bytes[start + row.len() - 1] = b'f';
        }
        bytes[offset] = replacement;
    }
}

#[test]
fn opening_repair_restores_the_color_key() {
    use crate::fixtures::{file, stream};
    let gray = "/ColorSpace /DeviceGray /Width 1 /Height 1 /BPC 8";
    let mut bytes = file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            stream(&format!("{gray} /SMask 4 0 R /Mask [128 128]"), &[128]),
            format!("<< {gray} /Length 5 0 R >>\nstream\nA\nendstream").into_bytes(),
            b"1".to_vec(),
            b"null".to_vec(),
        ],
        "",
    );
    replace_mask_on_repair(&mut bytes);
    let pdf = Pdf::new(bytes).unwrap();
    let obj = image(&pdf);
    assert!(pdf.xref().is_repaired());
    let decoded = obj.decoded_image(None).unwrap();
    assert_eq!(decoded.alpha.unwrap().luma.data, [0]);
}

#[test]
fn soft_mask_lookup_uses_the_table_repaired_on_open() {
    use crate::fixtures::{file, stream};
    let gray = "/ColorSpace /DeviceGray /Width 1 /Height 1 /BPC 8";
    for old_mask in [None, Some(37)] {
        let mut bytes = file(
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
                stream(&format!("{gray} /SMask 4 0 R /SMaskInData 5 0 R"), &[255]),
                old_mask.map_or_else(|| b"null".to_vec(), |v| stream(gray, &[v])),
                b"0".to_vec(),
                stream(gray, &[192]),
            ],
            "",
        );
        replace_mask_on_repair(&mut bytes);

        let pdf = Pdf::new(bytes).unwrap();
        let obj = image(&pdf);
        let initial = obj.stream.dict().get::<Stream<'_>>(b"SMask");
        assert_eq!(initial.map(|s| s.raw_data()[0]), Some(192));
        let decoded = obj.decoded_image(None).unwrap();
        assert!(pdf.xref().is_repaired());
        assert_eq!(decoded.alpha.unwrap().luma.data, [192]);
    }
}

#[test]
fn the_repaired_mask_supplies_the_matte_for_unblending() {
    use crate::fixtures::{file, stream};
    let gray = "/ColorSpace /DeviceGray /Width 1 /Height 1 /BPC 8";
    let mut bytes = file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            stream(&format!("{gray} /SMask 4 0 R"), &[128]),
            stream(&format!("{gray} /Matte 5 0 R"), &[64]),
            b"[0]".to_vec(),
            stream(&format!("{gray} /Matte [1]"), &[192]),
        ],
        "",
    );
    replace_mask_on_repair(&mut bytes);
    let pdf = Pdf::new(bytes).unwrap();
    assert!(pdf.xref().is_repaired());
    let decoded = image(&pdf).decoded_image(None).unwrap();
    let ImageData::Luma(color) = decoded.image else {
        panic!()
    };
    assert_eq!(color.data, [86]);
    assert_eq!(decoded.alpha.unwrap().luma.data, [192]);
}

fn request(obj: &ImageXObject<'_>, plane: Plane) -> ImageRegion {
    let asset = obj.asset(None);
    let mut source = asset.open(None).unwrap();
    assert!(source.is_streamed());
    let layout = source.layout().plane(plane).unwrap();
    let (w, h) = (layout.width, layout.height);
    source
        .request(&ImageRequest {
            plane,
            grid: (w, h),
            window: [0, 0, w, h],
            cancel: None,
        })
        .unwrap()
}

#[test]
fn short_images_convert_stored_zero() {
    for (dict, data, expected) in [
        (
            "/BPC 8 /Decode [1 0]",
            &[20, 30, 40][..],
            [235, 225, 215, 255, 255, 255],
        ),
        (
            "/BPC 16 /Decode [1 0]",
            &[255, 255, 255, 255, 255][..],
            [0, 0, 255, 255, 255, 255],
        ),
    ] {
        let pdf = pdf(&[(dict, data)]);
        let obj = image(&pdf);
        assert_eq!(request(&obj, Plane::Image).data, expected);
        let decoded = obj.decoded_image(None).unwrap();
        let ImageData::Luma(stored) = decoded.image else {
            panic!()
        };
        assert_eq!(
            stored.data,
            expected[..(stored.width * stored.height) as usize]
        );
        assert_eq!(decoded.tail.texel.as_deref(), Some(&[255][..]));
    }
}

#[test]
fn short_color_key_uses_source_precision_and_drops_the_incomplete_row() {
    let pdf = pdf(&[("/BPC 16 /Mask [256 256]", &[1, 0, 1, 1, 255])]);
    let obj = image(&pdf);
    assert_eq!(
        request(&obj, Plane::Image).data,
        [0, 0, 1, 255, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    let decoded = obj.decoded_image(None).unwrap();
    let (ImageData::Luma(whole), Some(alpha)) = (decoded.image, decoded.alpha.map(|a| a.luma))
    else {
        panic!()
    };
    assert_eq!(whole.data, [1, 1]);
    assert_eq!(alpha.data, [0, 255]);
}

#[test]
fn matte_does_not_paint_rows_missing_from_the_image() {
    let pdf = pdf(&[
        ("/BPC 8 /SMask 4 0 R", &[120, 90, 60]),
        ("/BPC 8 /Matte [0.5]", &[180, 100, 230, 200, 255, 0]),
    ]);
    let obj = image(&pdf);
    let region = request(&obj, Plane::Image);
    let decoded = obj.decoded_image(None).unwrap();
    let (ImageData::Luma(whole), Some(alpha)) = (decoded.image, decoded.alpha.map(|a| a.luma))
    else {
        panic!()
    };
    assert_eq!(whole.height, 1);
    let mut expected = Vec::new();
    for (v, a) in whole.data.into_iter().zip(alpha.data) {
        expected.extend([(u16::from(v) * u16::from(a) / 255) as u8, a]);
    }
    expected.resize(12, 0);
    assert_eq!(region.data, expected);
}

#[test]
fn independently_interpolated_mask_uses_its_own_plane() {
    let pdf = pdf(&[
        ("/BPC 8 /SMask 4 0 R", &[1, 2]),
        ("/BPC 8 /Interpolate true /Decode [1 0]", &[255]),
    ]);
    let obj = image(&pdf);
    assert_eq!(request(&obj, Plane::Image).data, [1, 2, 0, 0, 0, 0]);
    assert_eq!(
        request(&obj, Plane::Mask).data,
        [0, 255, 255, 255, 255, 255]
    );
}

#[test]
fn a_short_image_can_fit_its_full_declared_height() {
    let doc = pdf(&[("/BPC 8 /Height 2000000000 /Decode [1 0]", &[255, 255])]);
    let obj = image(&doc);
    assert_eq!(obj.height(), 2_000_000_000);
    let asset = obj.asset(None);
    let mut source = asset.open(None).unwrap();
    let region = source
        .request(&ImageRequest {
            plane: Plane::Image,
            grid: (1, 1),
            window: [0, 0, 1, 1],
            cancel: None,
        })
        .unwrap();
    assert_eq!(region.data, [255]);
}

#[test]
fn cancelling_a_request_allows_another_read_of_the_asset() {
    use std::sync::atomic::Ordering;
    let doc = pdf(&[("/BPC 8 /Filter /ASCIIHexDecode", b"102030405060>")]);
    let obj = image(&doc);
    let asset = obj.asset(None);
    let mut source = asset.open(None).unwrap();
    let token = asset.cancel_token();
    let request = ImageRequest {
        plane: Plane::Image,
        grid: (2, 3),
        window: [0, 2, 2, 3],
        cancel: None,
    };
    token.store(true, Ordering::Relaxed);
    assert!(matches!(
        source.request(&request),
        Err(RequestError::Cancelled)
    ));
    token.store(false, Ordering::Relaxed);
    assert_eq!(source.request(&request).unwrap().data, [0x50, 0x60]);
}

#[test]
fn external_image_ignores_embedded_samples() {
    let pdf = pdf(&[("/BPC 8 /F (elsewhere)", &[10, 20])]);
    let obj = image(&pdf);
    assert!(obj.decoded_image(None).is_none());
    assert!(obj.asset(None).open(None).is_none());
}

#[test]
fn sparse_region_means_match_padded_pixels() {
    for width in [30, 129, 2049] {
        let bytes = [0x10, 0x22, 0x34, 0x45];
        let doc = pdf(&[(&format!("/Width {width} /BPC 8 /Decode [1 0]"), &bytes)]);
        let obj = image(&doc);
        let asset = obj.asset(None);
        let mut source = asset.open(None).unwrap();
        let mut dense = vec![255; width as usize * 3];
        for (&b, v) in bytes.iter().zip(&mut dense) {
            *v = 255 - b;
        }
        for (grid, window) in [
            ((3, 2), [0, 0, 3, 2]),
            ((width, 3), [width - 3, 1, width, 3]),
            ((width, 3), [0, 0, 4, 1]),
            ((3, 2), [0, 0, 3, 2]),
        ] {
            let region = source
                .request(&ImageRequest {
                    plane: Plane::Image,
                    grid,
                    window,
                    cancel: None,
                })
                .unwrap();
            assert_eq!(
                region.data,
                crate::area_resample(&dense, [0, 0, width, 3], (width, 3), 1, grid, window)
            );
        }
    }
}

#[test]
fn four_transfer_functions_convert_an_empty_images_tail() {
    use crate::function::TransferFunction;
    use hayro_syntax::object::Object;
    let doc = pdf(&[(
        "/BPC 8 /TR << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>",
        &[],
    )]);
    let mut obj = image(&doc);
    let function =
        TransferFunction::new(&obj.stream.dict().get::<Object<'_>>(b"TR").unwrap()).unwrap();
    obj.transfer_function = Some(ActiveTransferFunction::Four(std::array::from_fn(|_| {
        function.clone()
    })));
    let decoded = obj.decoded_image(None).unwrap();
    assert_eq!(decoded.image.height(), 0);
    assert_eq!(decoded.tail.texel.as_deref(), Some(&[255, 255, 255][..]));
}

#[test]
fn whole_image_scales_an_independent_mask_over_the_color_tail() {
    let doc = pdf(&[
        (
            "/Width 1 /Height 4 /BPC 8 /CS /Unresolved /SMask 4 0 R",
            &[20, 30],
        ),
        ("/Width 1 /Height 2 /BPC 8", &[255, 255]),
    ]);
    let obj = image(&doc);
    let asset = obj.asset(None);
    let mut source = asset.open(None).unwrap();
    assert!(!source.is_streamed());
    let mask = source
        .request(&ImageRequest {
            plane: Plane::Mask,
            grid: (1, 2),
            window: [0, 0, 1, 2],
            cancel: None,
        })
        .unwrap();
    assert_eq!(mask.data, [255, 255]);
    assert_eq!(mask.to_image, Affine::scale_non_uniform(1., 2.));
    let mut bands = 0;
    obj.decoded_image(None).unwrap().bands(|_, _, alpha| {
        let (luma, transform) = alpha.unwrap();
        assert_eq!(transform, mask.to_image);
        assert_eq!(luma.data, mask.data);
        bands += 1;
    });
    assert_eq!(bands, 2);
}

#[test]
fn short_fax_preserves_mask_polarity_decode_and_tail() {
    for black_is_one in [false, true] {
        for invert in [false, true] {
            let decode = if invert { "[1 0]" } else { "[0 1]" };
            let dict = format!(
                "/Width 8 /Filter /CCITTFaxDecode /DecodeParms << /Columns 8 /BlackIs1 {black_is_one} >> /Decode {decode}"
            );
            let doc = pdf(&[(&dict, &[0x35, 0x14])]);
            let mut obj = image(&doc);
            let sample = u8::from(black_is_one ^ invert) * 255;
            let tail = u8::from(invert) * 255;
            let decoded = obj.decoded_image(None).unwrap();
            let ImageData::Luma(stored) = decoded.image else {
                panic!()
            };
            assert_eq!(stored.data, [sample; 8]);
            assert_eq!(stored.height, 1);
            assert_eq!(decoded.tail.declared, 3);
            assert_eq!(decoded.tail.texel.as_deref(), Some(&[tail][..]));
            for kind in [ImageKind::Mask, ImageKind::StencilMask] {
                obj.kind = kind;
                let mask = obj.decoded_mask(None).unwrap();
                let xor = if kind == ImageKind::StencilMask {
                    255
                } else {
                    0
                };
                assert_eq!(mask.luma.data, [sample ^ xor; 8]);
                assert_eq!(mask.tail.texel.as_deref(), Some(&[tail ^ xor][..]));
            }
        }
    }
}

#[test]
fn fax_color_keys_and_palette_indices_use_one_bit_samples() {
    let fax = "/Width 8 /Height 1 /BPC 1 /Filter /CCITTFaxDecode /DecodeParms << /Columns 8 /BlackIs1 true >>";
    for suffix in ["/Mask [1 1]", "/CS [/Indexed /DeviceGray 1 <1030>]"] {
        let doc = pdf(&[(&format!("{fax} {suffix}"), &[0x35, 0x14])]);
        let obj = image(&doc);
        let result = obj.decoded_image(None).unwrap();
        if suffix.starts_with("/Mask") {
            assert_eq!(result.alpha.unwrap().luma.data, [0; 8]);
        } else {
            let ImageData::Luma(stored) = result.image else {
                panic!()
            };
            assert_eq!(stored.data, [0x30; 8]);
        }
    }
}

#[test]
fn fax_transfer_function_maps_pixels_and_tail() {
    use crate::function::TransferFunction;
    use hayro_syntax::object::Object;
    let doc = pdf(&[(
        "/Width 8 /Filter /CCITTFaxDecode /DecodeParms << /Columns 8 /BlackIs1 true >> /TR << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>",
        &[0x35, 0x14],
    )]);
    let mut obj = image(&doc);
    let tr = TransferFunction::new(&obj.stream.dict().get::<Object<'_>>(b"TR").unwrap()).unwrap();
    obj.transfer_function = Some(ActiveTransferFunction::Single(tr));
    let result = obj.decoded_image(None).unwrap();
    let ImageData::Luma(stored) = result.image else {
        panic!()
    };
    assert_eq!(stored.data, [0; 8]);
    assert_eq!(result.tail.texel.as_deref(), Some(&[255][..]));
}
