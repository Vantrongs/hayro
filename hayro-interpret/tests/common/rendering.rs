use super::fixtures::{file, one_page, one_page_objects, stream};

pub(crate) fn group_annotation(on: bool) -> Vec<u8> {
    let mut objects = one_page_objects(32, "/Annots [4 0 R]");
    let state = if on { "ON" } else { "OFF" };
    objects[0] = format!("<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [5 0 R 7 0 R] /D << /BaseState /{state} /OFF [7 0 R] >> >> >>").into_bytes();
    objects.extend([
        b"<< /Subtype /Square /Rect [0 0 32 32] /OC 5 0 R /AP << /N 6 0 R >> >>".to_vec(),
        b"<< /Type /OCG >>".to_vec(),
        stream(
            "/Subtype /Form /BBox [0 0 32 32] /OC 7 0 R",
            b"0 1 0 rg 0 0 32 32 re f",
        ),
        b"<< /Type /OCG >>".to_vec(),
    ]);
    file(&objects, "")
}

pub(crate) fn short_image(height: u32) -> Vec<u8> {
    one_page(
        32,
        "/Contents 4 0 R /Resources << /XObject << /I 5 0 R >> >>",
        vec![
            stream("", b"32 0 0 32 0 0 cm /I Do"),
            stream(
                &format!(
                    "/Subtype /Image /ColorSpace /DeviceRGB /BitsPerComponent 8 /Decode [0 1 1 0 0 1] /Width 1 /Height {height}"
                ),
                &[255, 255, 0],
            ),
        ],
    )
}
