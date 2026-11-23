use crate::object::stream::{
    FilterResult, ImageColorSpace, ImageData, ImageDecodeParams, OwnedStream,
};
use crate::work::Meter;
use alloc::borrow::Cow;
use alloc::vec::Vec;

/// Decode JBIG2 data from a PDF stream.
///
/// `globals` is the stream with shared symbol dictionaries its parameters'
/// `JBIG2Globals` names.
pub(crate) fn decode(
    data: &[u8],
    globals: Option<&OwnedStream>,
    image_params: &ImageDecodeParams,
    meter: &Meter<'_>,
) -> Option<FilterResult<'static>> {
    let globals = globals.and_then(|g| {
        g.decoded_image_metered(&ImageDecodeParams::default(), meter)
            .map(|d| d.data)
            .ok()
    });
    meter.poll().ok()?;

    let settings = hayro_jbig2::DecodeSettings::default();
    let globals = globals
        .map(|globals| hayro_jbig2::Globals::new(&globals, &settings))
        .transpose()
        .ok()?;
    let page = hayro_jbig2::Page::new_embedded(data, globals.as_ref(), &settings).ok()?;
    let bitmap = page.decode().ok()?;

    // Expand to 8 bpc unless indexed, for easier RGBA8 conversion.
    // JBIG2 uses 1 for black, PDF uses 0.
    let (decoded, bpc) = if image_params.is_indexed {
        let width = bitmap.width() as usize;
        let row_bytes = width.div_ceil(8);
        let mut packed = meter.room(row_bytes.checked_mul(bitmap.height() as usize)?);
        let padding_mask = !0_u8 << ((8 - width % 8) % 8);

        for row in bitmap.rows() {
            packed.extend(bytes(row, row_bytes).map(|byte| !byte));

            meter.work(row_bytes).ok()?;
            if let Some(last) = packed.last_mut() {
                *last &= padding_mask;
            }
        }

        (packed, 1)
    } else {
        let pixels = bitmap.data_u8().ok()?;
        meter.work(pixels.len()).ok()?;
        (pixels, 8)
    };

    Some(FilterResult {
        data: Cow::Owned(decoded),
        image_data: Some(ImageData {
            alpha: None,
            color_space: Some(ImageColorSpace::Gray),
            component_depths: Vec::new(),
            bits_per_component: bpc,
            width: bitmap.width(),
            height: bitmap.height(),
        }),
    })
}

fn bytes(row: &[u32], len: usize) -> impl Iterator<Item = u8> + '_ {
    row.iter().flat_map(|word| word.to_be_bytes()).take(len)
}
