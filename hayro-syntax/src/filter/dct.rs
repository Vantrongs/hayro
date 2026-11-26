use crate::object::Dict;
use crate::object::dict::keys::COLOR_TRANSFORM;
use crate::object::stream::{FilterResult, ImageColorSpace, ImageData, ImageDecodeParams};
use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::num::NonZeroU32;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::colorspace::ColorSpace::CMYK;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::sync::CancelArc;
use core::sync::atomic::{AtomicBool, Ordering};
use zune_jpeg::{JpegDecoder, ScanlineReadStatus, ScanlineStatus};

/// A forward reader of native JPEG samples after the PDF color transform.
pub struct JpegRows<'a> {
    decoder: JpegDecoder<ZCursor<Cow<'a, [u8]>>>,
    colorspace: ColorSpace,
    stride: usize,
    streaming: bool,
}

pub(crate) fn decode(
    data: &[u8],
    params: &Dict<'_>,
    image_params: &ImageDecodeParams,
) -> Option<FilterResult<'static>> {
    let mut rows = JpegRows::new(Cow::Borrowed(data), params, image_params, None)?;
    let frame_bytes = rows.stride.checked_mul(rows.decoder.dimensions()?.1)?;
    if !rows.streaming || data.len() >= frame_bytes.div_ceil(16) {
        return rows.decode_whole();
    }
    let mut output = alloc::vec![0; rows.stride.checked_mul(16)?];
    let mut hint = Some(frame_bytes);
    let mut samples = Vec::new();
    loop {
        match rows.read(&mut output) {
            Some(0) => break,
            Some(n) => {
                samples.extend_from_slice(&output[..n * rows.stride]);
                super::reserve_observed(&mut samples, &mut hint);
            }
            None if samples.len() < frame_bytes.div_ceil(16) => {
                return (!samples.is_empty()).then(|| rows.result(samples));
            }
            None => {
                drop(samples);
                drop(rows);
                let whole = JpegRows::new(Cow::Borrowed(data), params, image_params, None)?;
                return whole.decode_whole();
            }
        }
    }
    Some(rows.result(samples))
}

impl<'a> JpegRows<'a> {
    /// Opens the JPEG data with its PDF decode parameters and optional cancellation token.
    pub fn new(
        data: Cow<'a, [u8]>,
        params: &Dict<'_>,
        image_params: &ImageDecodeParams,
        cancel: Option<CancelArc<AtomicBool>>,
    ) -> Option<Self> {
        if image_params.width > u16::MAX as u32 || image_params.height > u16::MAX as u32 {
            return None;
        }

        // Some JPEGs have bogus oversized dimensions, so patch them from the image dictionary.
        let data = maybe_patch_jpeg_dimensions(data, image_params)?;

        let streaming = find_marker(&data, true)
            .and_then(|i| data.get(i + 4))
            .copied();
        let options = DecoderOptions::default()
            .set_max_width(u16::MAX as usize)
            .set_max_height(u16::MAX as usize);
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
        if let Some(cancel) = cancel {
            decoder.set_cancel(move || cancel.load(Ordering::Relaxed));
        }
        decoder.decode_headers().ok()?;

        let color_transform = params.get::<u8>(COLOR_TRANSFORM);
        let input_color_space = decoder.input_colorspace().unwrap();

        let mut out_colorspace = if let Some(num_components) = image_params.num_components
            && !matches!(num_components, 1 | 3 | 4)
        {
            ColorSpace::MultiBand(NonZeroU32::new(num_components as u32)?)
        } else {
            match input_color_space {
                ColorSpace::YCbCr => {
                    if color_transform.is_none_or(|c| c == 1) {
                        ColorSpace::RGB
                    } else {
                        ColorSpace::YCbCr
                    }
                }
                ColorSpace::RGB | ColorSpace::RGBA => ColorSpace::RGB,
                ColorSpace::Luma | ColorSpace::LumaA => ColorSpace::Luma,
                // TODO: Find test case with color transform on cmyk
                CMYK => CMYK,
                ColorSpace::YCCK => ColorSpace::YCCK,
                _ => ColorSpace::RGB,
            }
        };

        // In case image had APP14 marker, we might have to override the colorspace.
        if input_color_space == CMYK && decoder.info().unwrap().components == 3 {
            out_colorspace = ColorSpace::RGB;
        }

        decoder.set_options(options.jpeg_set_out_colorspace(out_colorspace));
        let stride = decoder
            .dimensions()?
            .0
            .checked_mul(out_colorspace.num_components())?;
        let streaming = decoder.decoded_scans().is_none()
            && decoder.dimensions()?.1 != 0
            && streaming == Some(decoder.info()?.components);
        (stride != 0).then_some(Self {
            decoder,
            colorspace: out_colorspace,
            stride,
            streaming,
        })
    }

    pub(crate) fn streaming(self) -> Option<Self> {
        self.streaming.then_some(self)
    }

    fn decode_whole(mut self) -> Option<FilterResult<'static>> {
        let mut data = self.decoder.decode().ok()?;
        transform(self.colorspace, &mut data);
        Some(self.result(data))
    }

    /// Returns the number of sample bytes per row.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Reads complete rows, returning their count, zero at EOF, or None on failure.
    pub fn read(&mut self, output: &mut [u8]) -> Option<usize> {
        if output.len() < self.stride {
            return None;
        }
        let mut session = self.decoder.scanline_output();
        match session.read_scanlines(output, self.stride).ok()? {
            ScanlineReadStatus::RowsProcessed { rows } if rows > 0 => {
                transform(self.colorspace, &mut output[..rows * self.stride]);
                Some(rows)
            }
            ScanlineReadStatus::Complete if session.finish().ok()? == ScanlineStatus::Complete => {
                Some(0)
            }
            _ => None,
        }
    }

    /// The dimensions and sample format produced by the decoder.
    pub fn metadata(&self) -> ImageData {
        let width = self.decoder.dimensions().unwrap().0 as u32;
        let height = self.decoder.dimensions().unwrap().1 as u32;

        ImageData {
            alpha: None,
            color_space: match self.colorspace {
                ColorSpace::RGB | ColorSpace::YCbCr => Some(ImageColorSpace::Rgb),
                ColorSpace::Luma => Some(ImageColorSpace::Gray),
                ColorSpace::YCCK | CMYK => Some(ImageColorSpace::Cmyk),
                ColorSpace::MultiBand(_) => None,
                _ => None,
            },
            bits_per_component: 8,
            width,
            height,
        }
    }

    fn result(&self, decoded: Vec<u8>) -> FilterResult<'static> {
        FilterResult {
            data: Cow::Owned(decoded),
            image_data: Some(self.metadata()),
        }
    }
}

fn transform(out_colorspace: ColorSpace, decoded: &mut [u8]) {
    if out_colorspace == ColorSpace::YCCK {
        // See <https://github.com/mozilla/pdf.js/blob/69595a29192b7704733404a42a2ebb537601117b/src/core/jpg.js#L1331>
        for c in decoded.chunks_mut(4) {
            let y = c[0] as f32;
            let cb = c[1] as f32;
            let cr = c[2] as f32;
            c[0] = (434.456 - y - 1.402 * cr) as u8;
            c[1] = (119.541 - y + 0.344 * cb + 0.714 * cr) as u8;
            c[2] = (481.816 - y - 1.772 * cb) as u8;
        }
    }
}

fn maybe_patch_jpeg_dimensions<'a>(
    mut data: Cow<'a, [u8]>,
    image_params: &ImageDecodeParams,
) -> Option<Cow<'a, [u8]>> {
    let sof_offset = find_marker(&data, false)?;

    let height_offset = sof_offset.checked_add(5)?;
    let width_offset = sof_offset.checked_add(7)?;

    let jpeg_height = u16::from_be_bytes([
        *data.get(height_offset)?,
        *data.get(height_offset.checked_add(1)?)?,
    ]);
    let jpeg_width = u16::from_be_bytes([
        *data.get(width_offset)?,
        *data.get(width_offset.checked_add(1)?)?,
    ]);

    let jpeg_area = (jpeg_width as usize).checked_mul(jpeg_height as usize)?;
    let image_area = (image_params.width as usize).checked_mul(image_params.height as usize)?;
    let need_patch = jpeg_area > image_area;

    if !need_patch {
        return Some(data);
    }

    let target_w = (image_params.width as u16).to_be_bytes();
    let target_h = (image_params.height as u16).to_be_bytes();

    let patched = data.to_mut();
    patched[height_offset..height_offset.checked_add(2)?].copy_from_slice(&target_h);
    patched[width_offset..width_offset.checked_add(2)?].copy_from_slice(&target_w);

    Some(data)
}

fn find_marker(data: &[u8], sos: bool) -> Option<usize> {
    let mut i = 0_usize;

    while i.checked_add(1).is_some_and(|next| next < data.len()) {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }

        let marker = data[i + 1];

        match marker {
            // SOF0–SOF15, excluding DHT, JPG and DAC.
            0xDA if sos => return Some(i),
            0xC0..=0xCF if !sos && marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
                return Some(i);
            }
            0xFF => {
                i += 1;

                continue;
            }
            0xD8 | 0xD9 | 0x01 | 0x00 => {
                i += 2;

                continue;
            }
            _ => {
                let len_start = i.checked_add(2)?;
                let len_end = i.checked_add(3)?;
                let seg_len =
                    u16::from_be_bytes([*data.get(len_start)?, *data.get(len_end)?]) as usize;

                i = i.checked_add(2)?.checked_add(seg_len)?;
            }
        }
    }

    None
}
