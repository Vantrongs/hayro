use super::{
    DecodeContext, Rows, decode_context, into_owned, invert as invert_bytes, pixel_piece,
    repeat_last_row, unpack_samples,
};
use crate::LumaData;
use crate::color::ColorSpace;
use crate::function::interpolate;
use crate::x_object::image::{ImageKind, ImageXObject};
use hayro_syntax::work::{Meter, POLL_BYTES};
use std::sync::LazyLock;

pub(crate) static BILEVEL_MASK_LUT: LazyLock<Box<[u64; 256]>> = LazyLock::new(|| {
    let mut lut = Box::new([0; 256]);
    for (byte, expanded) in lut.iter_mut().enumerate() {
        let mut pixels = [0; 8];
        for (bit, pixel) in pixels.iter_mut().enumerate() {
            *pixel = if byte & (0x80 >> bit) != 0 { 255 } else { 0 };
        }
        *expanded = u64::from_ne_bytes(pixels);
    }
    lut
});

pub(crate) struct DecodedMask {
    /// The alpha: the rows its data holds, and those `rows` covers past them.
    pub(crate) luma: LumaData,
    pub(crate) rows: Rows,
}

impl DecodedMask {
    /// The alpha with every row it covers, the last it holds repeated: as large as
    /// the mask intact.
    pub(crate) fn into_whole(self) -> LumaData {
        let mut luma = self.luma;
        repeat_last_row(
            &mut luma.data,
            luma.width as usize,
            self.rows.declared,
            &Meter::default(),
        );
        luma.height = self.rows.declared;
        luma
    }
}

/// The mask decoded whole, its work counted on `meter`; `None` also once the meter's
/// hook asks to stop.
pub(crate) fn decode_mask(
    obj: &ImageXObject<'_>,
    target_dimension: Option<(u32, u32)>,
    meter: &Meter<'_>,
) -> Option<DecodedMask> {
    let ctx = decode_context(obj, target_dimension, false, meter)?;
    let (width, rows) = (ctx.width, ctx.rows);
    let scale_factors = ctx.scale_factors;

    // Note: The semantics between "normal" soft masks (i.e. masks defined in
    // the graphics state or via `Mask`/`SMask` are inverted compared to
    // stencil masks (defined via `ImageMask`). The former match the semantics
    // of normal alpha images, where 0 stands for invisible and MAX stands for
    // fully opaque. For stencil masks, it's the other way around: 1 means the
    // paint is visible, while 0 means it's invisible.
    let invert = obj.kind == ImageKind::StencilMask;
    let (data, height) = decode_mask_data(ctx, invert, meter)?;

    Some(DecodedMask {
        luma: LumaData {
            data,
            width,
            height,
            interpolate: obj.interpolate,
            scale_factors,
        },
        rows,
    })
}

/// How a mask's samples become alpha (`decode_mask_data`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MaskPath {
    /// One bit per sample, each 0 or 255, inverted or not.
    Bilevel { invert: bool },
    /// One byte per sample, inverted or not.
    Bytes { invert: bool },
    /// Through the decode array.
    Decoded,
}

impl MaskPath {
    /// The path for samples of `color_space` (one component, if a fast path applies)
    /// with `bits_per_component` and `decode_arr`; `invert` for a stencil mask.
    pub(crate) fn new(
        color_space: &ColorSpace,
        bits_per_component: u8,
        decode_arr: &[(f32, f32)],
        invert: bool,
    ) -> Self {
        let default_decode = color_space.default_decode_arr(bits_per_component as f32);
        let inverted_default = color_space.inverted_default_decode_arr(bits_per_component as f32);
        let inverted = decode_arr == inverted_default.as_slice();
        let fast = decode_arr == default_decode.as_slice() || inverted;
        if bits_per_component == 1 && color_space.num_components() == 1 && fast {
            Self::Bilevel {
                invert: invert ^ inverted,
            }
        } else if bits_per_component == 8 && fast {
            Self::Bytes {
                invert: invert ^ inverted,
            }
        } else {
            Self::Decoded
        }
    }
}

/// The alpha a mask's stored sample `value` of `bits` becomes through the
/// component's `decode` entry; `invert` for a stencil mask.
fn mask_alpha(value: u32, bits: u8, (decode_min, decode_max): (f32, f32), invert: bool) -> u8 {
    let source_max = 2.0_f32.powi(i32::from(bits)) - 1.0;
    let value = interpolate(value as f32, 0.0, source_max, decode_min, decode_max);
    let value = if invert { 1.0 - value } else { value };
    (value * 255.0 + 0.5) as u8
}

/// For a one-component mask of at most eight bits a sample whose samples go
/// through the decode array ([`MaskPath::Decoded`]), the alpha each stored value
/// becomes, as decoding the mask whole maps it (a sample the data does not hold whole
/// as stored zero); `None` without a decode entry, where decoding it whole makes no
/// alpha.
pub(crate) fn mask_lut(bits: u8, decode_arr: &[(f32, f32)], invert: bool) -> Option<[u8; 256]> {
    let &decode = decode_arr.first()?;
    if !(1..=8).contains(&bits) {
        return None;
    }
    let mut lut = [0; 256];
    for (v, alpha) in lut.iter_mut().enumerate().take(1 << bits) {
        *alpha = mask_alpha(v as u32, bits, decode, invert);
    }
    Some(lut)
}

fn decode_mask_data(
    ctx: DecodeContext<'_>,
    invert: bool,
    meter: &Meter<'_>,
) -> Option<(Vec<u8>, u32)> {
    let path = MaskPath::new(
        &ctx.color_space,
        ctx.bits_per_component,
        &ctx.decode_arr,
        invert,
    );

    let data = match path {
        MaskPath::Bilevel { invert } => decode_bilevel_mask_data(&ctx, invert, meter)?,
        MaskPath::Bytes { invert } => {
            let mut decoded = into_owned(ctx.decoded.data, meter)?;
            if invert {
                invert_bytes(&mut decoded, meter)?;
            }

            decoded
        }
        MaskPath::Decoded => {
            let num_components = ctx.color_space.num_components() as usize;
            let components = unpack_samples(
                &ctx.decoded.data,
                ctx.width,
                ctx.height,
                num_components,
                ctx.bits_per_component,
                meter,
            )?;

            let mut decoded = Vec::with_capacity(components.len());

            // Whole pixels a piece, as `chunks` splits them.
            let piece_len = pixel_piece(num_components) * num_components.max(1);
            for piece in components.chunks(piece_len) {
                for pixel in piece.chunks(num_components) {
                    for (component, &decode) in pixel.iter().zip(&ctx.decode_arr) {
                        decoded.push(mask_alpha(
                            u32::from(*component),
                            ctx.bits_per_component,
                            decode,
                            invert,
                        ));
                    }
                }
                meter.work(piece.len()).ok()?;
            }

            decoded
        }
    };

    Some((data, ctx.height))
}

fn decode_bilevel_mask_data(
    ctx: &DecodeContext<'_>,
    should_invert: bool,
    meter: &Meter<'_>,
) -> Option<Vec<u8>> {
    let width = ctx.width as usize;
    let height = ctx.height as usize;
    let row_bytes = width.div_ceil(8);
    let data = ctx.decoded.data.as_ref();

    let xor_mask = if should_invert { u64::MAX } else { 0 };
    let full_bytes = width / 8;
    let tail_bits = width % 8;
    let lut = &**BILEVEL_MASK_LUT;
    let mut out = Vec::with_capacity(width.saturating_mul(height));
    // To avoid repeatedly calling `extend`, which turns out to be very
    // expensive.
    let mut buffer = [[0; 8]; 32];

    for row in data.chunks(row_bytes).take(height) {
        for piece in row[..row.len().min(full_bytes)].chunks(POLL_BYTES / 8) {
            for chunk in piece.chunks(buffer.len()) {
                for (expanded, &byte) in buffer.iter_mut().zip(chunk) {
                    *expanded = (lut[byte as usize] ^ xor_mask).to_ne_bytes();
                }
                out.extend_from_slice(buffer[..chunk.len()].as_flattened());
            }
            meter.work(piece.len() * 8).ok()?;
        }

        if tail_bits != 0 && row.len() > full_bytes {
            let expanded = (lut[row[full_bytes] as usize] ^ xor_mask).to_ne_bytes();
            out.extend_from_slice(&expanded[..tail_bits]);
        }
        meter.work(1).ok()?;
    }
    Some(out)
}
