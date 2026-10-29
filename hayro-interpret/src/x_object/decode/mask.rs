use super::{
    DecodeContext, Held, Rows, complete, decode_context, into_owned, invert as invert_bytes,
    pixel_piece, unpack_samples,
};
use crate::LumaData;
use crate::color::ColorSpace;
use crate::function::interpolate;
use crate::x_object::image::{ImageKind, ImageXObject};
use hayro_syntax::object::stream::FilterResult;
use hayro_syntax::work::{Meter, POLL_BYTES};
use std::borrow::Cow;
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
    /// The alpha: the texels its data holds, and past them what `rows` says.
    luma: LumaData,
    rows: Rows,
}

impl DecodedMask {
    /// `luma` covering `rows`; `None` unless it holds the texels of the rows it says
    /// ([`Rows::hold`]).
    fn new(luma: LumaData, rows: Rows) -> Option<Self> {
        rows.hold(luma.data.len(), luma.width, luma.height, 1)
            .then_some(Self { luma, rows })
    }

    pub(crate) fn luma(&self) -> &LumaData {
        &self.luma
    }

    /// The alpha held, and its fill past it.
    pub(crate) fn held(&self) -> Held<'_> {
        let fill = self.rows.fill.as_ref().map(|f| &f[..1]);
        Held::new(&self.luma.data, self.luma.width, 1, fill)
    }

    pub(crate) fn rows(&self) -> Rows {
        self.rows
    }

    pub(crate) fn into_parts(self) -> (LumaData, Rows) {
        (self.luma, self.rows)
    }

    /// The alpha with every texel of the rows it covers, those past the texels held
    /// its fill: as large as the mask intact.
    pub(crate) fn into_whole(self) -> LumaData {
        let mut luma = self.luma;
        if let Some(fill) = self.rows.fill {
            let texels = luma.width as usize * self.rows.declared as usize;
            complete(&mut luma.data, texels, &fill[..1], &Meter::default());
            luma.height = self.rows.declared;
        }
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
    let ctx = decode_context(obj, target_dimension, meter)?;
    let width = ctx.width;
    let scale_factors = ctx.scale_factors;

    // Note: The semantics between "normal" soft masks (i.e. masks defined in
    // the graphics state or via `Mask`/`SMask` are inverted compared to
    // stencil masks (defined via `ImageMask`). The former match the semantics
    // of normal alpha images, where 0 stands for invisible and MAX stands for
    // fully opaque. For stencil masks, it's the other way around: 1 means the
    // paint is visible, while 0 means it's invisible.
    let invert = obj.kind == ImageKind::StencilMask;
    // The alpha of a sample the data does not hold: a stored zero sample's.
    let fill = match ctx.transparent {
        true => None,
        false => {
            let n = ctx.color_space.num_components() as usize;
            let bytes = (n * usize::from(ctx.bits_per_component)).div_ceil(8);
            let one = DecodeContext {
                decoded: FilterResult {
                    data: Cow::Owned(vec![0; bytes]),
                    image_data: None,
                },
                width: 1,
                height: 1,
                declared: 1,
                transparent: false,
                scale_factors,
                color_space: ctx.color_space.clone(),
                bits_per_component: ctx.bits_per_component,
                decode_arr: ctx.decode_arr.clone(),
            };
            let (alpha, _) = decode_mask_data(one, invert, meter)?;
            Some([*alpha.first()?, 0, 0])
        }
    };
    let rows = Rows {
        declared: ctx.declared,
        fill,
    };
    let (data, height) = decode_mask_data(ctx, invert, meter)?;

    DecodedMask::new(
        LumaData {
            data,
            width,
            height,
            interpolate: obj.interpolate,
            scale_factors,
        },
        rows,
    )
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
/// component's `decode` entry; `invert` for a stencil mask, whose sample is inverted
/// before it is mapped, as `MuPDF` maps it (observed through `PyMuPDF` 1.28.2): as
/// inverting the value mapped where the entry's values sum to one.
fn mask_alpha(value: u32, bits: u8, (decode_min, decode_max): (f32, f32), invert: bool) -> u8 {
    let source_max = 2.0_f32.powi(i32::from(bits)) - 1.0;
    let value = if invert {
        source_max - value as f32
    } else {
        value as f32
    };
    let value = interpolate(value, 0.0, source_max, decode_min, decode_max);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A decoded mask holds a byte for each texel of the rows it says it holds, the
    /// last held in part only where a fill stands for the texels past it: one of no
    /// bytes claiming a row, or holding more rows than it covers, is never made.
    #[test]
    fn a_mask_holding_other_rows_than_it_claims_is_not_made() {
        let luma = |data: Vec<u8>, height| LumaData {
            data,
            width: 2,
            height,
            interpolate: false,
            scale_factors: (1.0, 1.0),
        };
        let rows = |declared| Rows {
            declared,
            fill: None,
        };
        assert!(DecodedMask::new(luma(vec![], 1), rows(1)).is_none());
        assert!(DecodedMask::new(luma(vec![0; 3], 2), rows(2)).is_none());
        assert!(DecodedMask::new(luma(vec![0; 4], 2), rows(1)).is_none());
        assert!(DecodedMask::new(luma(vec![], 0), rows(1)).is_none());
        assert!(DecodedMask::new(luma(vec![0; 2], 1), rows(5)).is_some());
        let filled = |declared| Rows {
            declared,
            fill: Some([0; 3]),
        };
        assert!(DecodedMask::new(luma(vec![0; 3], 2), filled(2)).is_some());
        assert!(DecodedMask::new(luma(vec![], 0), filled(1)).is_some());
        assert!(DecodedMask::new(luma(vec![0; 3], 1), filled(2)).is_none());
        assert!(DecodedMask::new(luma(vec![0; 3], 2), filled(1)).is_none());
    }
}
