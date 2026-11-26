use super::*;
use hayro_syntax::Pdf;
use hayro_syntax::object::{ObjectIdentifier, Stream};

fn pdf(images: &[(&str, &[u8])]) -> Pdf {
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

fn image(pdf: &Pdf) -> ImageXObject<'_> {
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
        let (ImageData::Luma(whole), None) = obj.decoded_image(None).unwrap().into_whole().unwrap()
        else {
            panic!()
        };
        assert_eq!(whole.data, expected);
        assert_eq!(whole.height, 3);
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
    let (ImageData::Luma(whole), Some(alpha)) =
        obj.decoded_image(None).unwrap().into_whole().unwrap()
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
    let (ImageData::Luma(whole), Some(alpha)) =
        obj.decoded_image(None).unwrap().into_whole().unwrap()
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
