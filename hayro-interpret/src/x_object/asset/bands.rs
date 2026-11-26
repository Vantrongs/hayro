use super::*;
use crate::color::{ColorComponents, ToLuma};
use crate::x_object::decode::{convert_band, premultiply, unblend_matte};
use hayro_syntax::Filter;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::{
    DecodedReader, ImageData as Metadata, ImageDecodeParams, JpegRows,
};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::AtomicBool;
use tempfile::{SpooledTempFile, spooled_tempfile};

const BAND_BYTES: usize = 128 * 1024;

enum Producer<'a> {
    Direct(u64),
    Finished,
    Packed(Vec<u8>),
    Jpeg(Box<JpegRows<'a>>),
    Filter(Box<DecodedReader<'a>>),
}

self_cell::self_cell!(
    struct StreamRows {
        owner: OwnedStream,
        #[not_covariant]
        dependent: Producer,
    }
);

pub(super) struct Bands {
    reader: StreamRows,
    scratch: Vec<u8>,

    token: Arc<AtomicBool>,
    file: SpooledTempFile,
    bytes: u64,
    eof: bool,
    stride: usize,
    pixel_bits: usize,
    metadata: Option<Metadata>,
    bits: u8,
    keyed: bool,
    mask: Option<Box<Self>>,
    matte: Option<[u8; 3]>,
    sample_format: PixelFormat,
    pub(super) layout: ImageLayout,
}

pub(super) struct Band {
    pub(super) data: Option<Vec<u8>>,
    pub(super) width: usize,
    pub(super) rows: u32,
    pub(super) bytes: u64,
}

impl Bands {
    pub(super) fn new(asset: &ImageAsset, obj: &ImageXObject<'_>) -> Option<Self> {
        Self::from_stream(asset.0.stream.clone(), obj, asset.cancel_token())
    }

    fn from_stream(
        owner: OwnedStream,
        obj: &ImageXObject<'_>,
        token: Arc<AtomicBool>,
    ) -> Option<Self> {
        let dict = obj.stream.dict();
        if dict.get::<u8>(SMASK_IN_DATA).unwrap_or(0) != 0 {
            return None;
        }
        let external = if obj.kind == ImageKind::Image {
            dict.get::<hayro_syntax::object::Stream<'_>>(SMASK)
                .or_else(|| dict.get::<hayro_syntax::object::Stream<'_>>(MASK))
        } else {
            None
        };
        let mask = match external.as_ref() {
            Some(stream) => {
                let obj = ImageXObject::new_mask(stream, &obj.warning_sink, &obj.cache)?;
                Some(Box::new(Self::from_stream(
                    stream.to_owned_stream(),
                    &obj,
                    token.clone(),
                )?))
            }
            None => None,
        };
        let cs = obj.color_space.as_ref()?;
        let keyed = super::super::decode::short_rows_transparent(obj, cs.num_components());
        let channels = usize::from(cs.num_components());
        let bpc = dict
            .get::<u8>(BPC)
            .or_else(|| dict.get::<u8>(BITS_PER_COMPONENT))
            .unwrap_or(if obj.kind == ImageKind::StencilMask {
                1
            } else {
                8
            });
        let params = ImageDecodeParams {
            is_indexed: cs.is_indexed(),
            num_components: Some(cs.num_components()),
            width: obj.width(),
            height: obj.height(),
            ..Default::default()
        };
        let mut metadata = None;
        let reader = StreamRows::try_new(owner, |owned| {
            let stream = owned.get().ok_or(())?;
            let filters = stream.filters();
            if filters.is_empty() || filters.as_slice() == [Filter::Crypt] {
                let mut reader = stream.decrypted_reader().map_err(|_| ())?;
                if reader.can_seek() {
                    return Ok(Producer::Direct(reader.skip(usize::MAX) as u64));
                }
            }
            if filters.last() == Some(&Filter::CcittFaxDecode) {
                let fax = stream.fax_image(&params, Some(token.clone())).ok_or(())?;
                let info = fax.image_data.ok_or(())?;
                if params.width != info.width || params.height != info.height || channels != 1 {
                    return Err(());
                }
                metadata = Some(info);
                return Ok(Producer::Packed(fax.data.into_owned()));
            }
            if filters.last() == Some(&Filter::DctDecode) {
                let jpeg = stream.jpeg_rows(&params, Some(token.clone())).ok_or(())?;
                let info = jpeg.metadata();
                if info.width != params.width
                    || info.height != params.height
                    || jpeg.stride() != info.width as usize * channels
                {
                    return Err(());
                }
                metadata = Some(info);
                Ok(Producer::Jpeg(Box::new(jpeg)))
            } else {
                Ok(Producer::Filter(Box::new(
                    stream.decoded_reader(Some(token.clone())).map_err(|_| ())?,
                )))
            }
        })
        .ok()?;
        let bits = metadata.as_ref().map_or(bpc, |i| i.bits_per_component);
        if !matches!(bits, 1..=16) {
            return None;
        }
        let stride = (params.width as usize)
            .checked_mul(channels)?
            .checked_mul(usize::from(bits))?
            .div_ceil(8);
        if stride == 0 {
            return None;
        }
        let gray = obj
            .transfer_function
            .as_ref()
            .is_none_or(|tf| matches!(tf, ActiveTransferFunction::Single(_)))
            && cs.to_luma(&mut []).is_some();
        let matte = dict
            .get::<hayro_syntax::object::Stream<'_>>(SMASK)
            .and_then(|stream| stream.dict().get::<ColorComponents>(MATTE))
            .filter(|values| values.len() == channels)
            .map(|values| {
                let mut rgb = [0; 3];
                cs.convert_values(&values, &mut rgb);
                rgb
            });
        let mut layout = ImageLayout {
            image: PlaneLayout {
                width: params.width,
                height: params.height,
                to_image: (1., 1.),
                interpolate: obj.interpolate,
                format: match (obj.kind, gray, keyed && external.is_none()) {
                    (ImageKind::Image, true, true) => PixelFormat::LumaAlpha,
                    (ImageKind::Image, false, true) => PixelFormat::RgbAlpha,
                    (ImageKind::Image, true, false) => PixelFormat::Luma,
                    (ImageKind::Image, false, false) => PixelFormat::Rgb,
                    _ => PixelFormat::Alpha,
                },
            },
            mask: None,
        };
        let sample_format = layout.image.format;
        if let Some(m) = &mask {
            let plane = m.layout.image;
            if plane.width == params.width
                && plane.height == params.height
                && plane.interpolate == obj.interpolate
            {
                layout.image.format = sample_format.with_alpha();
            } else {
                if matte.is_some() {
                    return None;
                }
                layout.mask = Some(PlaneLayout {
                    to_image: (
                        f64::from(params.width) / f64::from(plane.width),
                        f64::from(params.height) / f64::from(plane.height),
                    ),
                    format: PixelFormat::Alpha,
                    ..plane
                });
            }
        }
        Some(Self {
            mask,
            matte,
            sample_format,
            reader,
            token,
            scratch: Vec::new(),

            file: spooled_tempfile(BAND_BYTES),
            bytes: 0,
            eof: false,
            stride,
            pixel_bits: channels * usize::from(bits),
            metadata,
            bits,
            keyed,
            layout,
        })
    }

    fn advance(&mut self, end: u32) -> Result<(bool, u32, u64), RequestError> {
        use std::sync::atomic::Ordering;
        if self.token.load(Ordering::Relaxed) {
            return Err(RequestError::Cancelled);
        }

        let end_bytes = u64::from(end).saturating_mul(self.stride as u64);
        let count = self.band_rows();
        let direct = self
            .reader
            .with_dependent(|_, r| matches!(r, Producer::Direct(_) | Producer::Packed(_)));
        let mut advanced = 0;
        let mut inflated = 0;
        if !direct && self.bytes < end_bytes && !self.eof {
            let result = self.reader.with_dependent_mut(|_, r| match r {
                Producer::Jpeg(jpeg) => {
                    self.scratch.resize(self.stride * count, 0);
                    jpeg.read(&mut self.scratch).map(|n| n * self.stride)
                }
                Producer::Filter(filter) => {
                    self.scratch
                        .resize((self.stride * count).min(BAND_BYTES), 0);
                    let before = filter.inflated_bytes();
                    let result = filter.read(&mut self.scratch).ok();
                    inflated = filter.inflated_bytes() - before;
                    result
                }
                _ => unreachable!(),
            });
            let Some(n) = result else {
                if self.token.load(Ordering::Relaxed) {
                    return Err(RequestError::Cancelled);
                }

                return Err(RequestError::Failed);
            };
            if n == 0 {
                self.eof = true;
            } else {
                if self
                    .file
                    .seek(SeekFrom::Start(self.bytes))
                    .and_then(|_| self.file.write_all(&self.scratch[..n]))
                    .is_err()
                {
                    return Err(RequestError::Failed);
                }
                let previous = self.bytes;
                self.bytes += n as u64;
                advanced = (self.bytes / self.stride as u64 - previous / self.stride as u64) as u32;
                if self.bytes < end_bytes {
                    return Ok((false, advanced, inflated));
                }
            }
            if self.eof
                || self.bytes
                    >= u64::from(self.layout.image.height).saturating_mul(self.stride as u64)
            {
                self.reader
                    .with_dependent_mut(|_, r| *r = Producer::Finished);
                self.scratch = Vec::new();
                self.eof = true;
            }
        }
        Ok((true, advanced, inflated))
    }

    fn samples(
        &mut self,
        columns: &std::ops::Range<u32>,
        first: u32,
        end: u32,
        obj: &ImageXObject<'_>,
    ) -> Result<Vec<u8>, RequestError> {
        let stored = self.stored_bytes();
        let start = u64::from(first).saturating_mul(self.stride as u64);
        let available = usize::try_from(stored.saturating_sub(start)).unwrap_or(usize::MAX);
        let held =
            super::super::decode::fit_rows(available, self.stride, end - first, self.bits, || {
                self.keyed
            });
        let rows = if self.keyed {
            held / self.stride
        } else {
            (end - first) as usize
        };

        let alignment = 8 / (1 << self.pixel_bits.trailing_zeros().min(3));
        let left = columns.start as usize / alignment * alignment;
        let width = columns.end as usize - left;
        let skip = left * self.pixel_bits / 8;
        let row_len = (width * self.pixel_bits).div_ceil(8);
        let mut native = vec![0; rows * row_len];
        self.reader
            .with_dependent(|_, r| -> Result<(), RequestError> {
                let mut direct = if matches!(r, Producer::Direct(_)) {
                    Some(
                        obj.stream
                            .decrypted_reader()
                            .map_err(|_| RequestError::Failed)?,
                    )
                } else {
                    None
                };
                let mut position = 0;
                let mut copy = |at: usize, target: &mut [u8]| -> Result<(), RequestError> {
                    let n = held.saturating_sub(at).min(target.len());
                    if n == 0 {
                        return Ok(());
                    }
                    let offset = start.checked_add(at as u64).ok_or(RequestError::Failed)?;
                    let target = &mut target[..n];
                    if let Some(reader) = &mut direct {
                        reader.skip((offset - position) as usize);
                        reader.read(target);
                        position = offset + n as u64;
                    } else if let Producer::Packed(data) = r {
                        target.copy_from_slice(&data[offset as usize..offset as usize + n]);
                    } else {
                        self.file
                            .seek(SeekFrom::Start(offset))
                            .and_then(|_| self.file.read_exact(target))
                            .map_err(|_| RequestError::Failed)?;
                    }
                    Ok(())
                };
                if row_len == self.stride && skip == 0 {
                    copy(0, &mut native)?;
                } else {
                    for (row, dst) in native.chunks_exact_mut(row_len).enumerate() {
                        copy(row * self.stride + skip, dst)?;
                    }
                }
                Ok(())
            })?;
        let c = self.sample_format.channels();
        let mut output = if rows == 0 {
            Vec::new()
        } else {
            let (data, channels) = convert_band(
                obj,
                native,
                self.metadata.clone(),
                width as u32,
                rows as u32,
            )
            .ok_or(RequestError::Failed)?;
            if channels != c {
                return Err(RequestError::Failed);
            }
            data
        };
        let clipped = (columns.start as usize - left) * c;
        let out_stride = columns.len() * c;
        if clipped > 0 {
            for row in 0..rows {
                let offset = row * width * c + clipped;
                output.copy_within(offset..offset + out_stride, row * out_stride);
            }
            output.truncate(rows * out_stride);
        }
        if self.keyed {
            output.resize((end - first) as usize * out_stride, 0);
        }
        Ok(output)
    }

    fn prefix(&self, first: u32) -> u32 {
        let bytes = self
            .stored_bytes()
            .saturating_sub(u64::from(first) * self.stride as u64)
            .min(self.stride as u64);
        (bytes * 8)
            .div_ceil(self.pixel_bits as u64)
            .min(u64::from(self.layout.image.width)) as u32
    }

    pub(super) fn read_band(
        &mut self,
        plane: Plane,
        columns: &std::ops::Range<u32>,
        first: u32,
        end: u32,
        obj: &ImageXObject<'_>,
    ) -> Result<Band, RequestError> {
        let combined = self.mask.is_some() && self.layout.mask.is_none();
        let (ready, mut rows, mut bytes) = if plane == Plane::Image {
            self.advance(end)?
        } else {
            (true, 0, 0)
        };
        let mut prefix = if plane == Plane::Image {
            self.prefix(first)
        } else {
            0
        };
        let mut ready = ready;
        if ready && (plane == Plane::Mask || combined) {
            let mask = self.mask.as_mut().ok_or(RequestError::Failed)?;
            let (done, m, n) = mask.advance(end)?;
            ready = done;
            rows += m;
            bytes += n;
            prefix = prefix.max(mask.prefix(first));
        }
        let mut band = Band {
            data: None,
            width: 0,
            rows,
            bytes,
        };
        if ready {
            let end_col = prefix.max(columns.start).saturating_add(1).min(columns.end);
            let cropped = columns.start..end_col;
            band.width = cropped.len();
            band.data = Some(self.read_samples(plane, &cropped, first, end, obj)?);
        }
        Ok(band)
    }

    fn read_samples(
        &mut self,
        plane: Plane,
        columns: &std::ops::Range<u32>,
        first: u32,
        end: u32,
        obj: &ImageXObject<'_>,
    ) -> Result<Vec<u8>, RequestError> {
        if self.mask.is_none() || plane == Plane::Image && self.layout.mask.is_some() {
            return self.samples(columns, first, end, obj);
        }
        let stream = obj
            .stream
            .dict()
            .get::<hayro_syntax::object::Stream<'_>>(SMASK)
            .or_else(|| {
                obj.stream
                    .dict()
                    .get::<hayro_syntax::object::Stream<'_>>(MASK)
            })
            .ok_or(RequestError::Failed)?;
        let mask_obj = ImageXObject::new_mask(&stream, &obj.warning_sink, &obj.cache)
            .ok_or(RequestError::Failed)?;
        let mask = self.mask.as_mut().unwrap();
        if plane == Plane::Mask {
            return mask.samples(columns, first, end, &mask_obj);
        }
        let mut alpha = mask.samples(columns, first, end, &mask_obj)?;
        let mut color = self.samples(columns, first, end, obj)?;
        if self.keyed {
            // Matte rows without stored colour have zero coverage even with an opaque mask.
            let bytes = self.stored_bytes();
            let rows = (bytes / self.stride as u64)
                .saturating_sub(u64::from(first))
                .min(u64::from(end - first));
            alpha[(rows as usize * columns.len())..].fill(0);
        }
        let channels = self.sample_format.channels();
        let mut data = Vec::with_capacity(color.len() + alpha.len());
        if let Some(matte) = self.matte {
            unblend_matte(&mut color, channels, &alpha, matte);
        }
        premultiply(&mut data, &color, channels, &alpha);
        Ok(data)
    }

    fn stored_bytes(&self) -> u64 {
        self.reader.with_dependent(|_, r| match r {
            Producer::Direct(len) => *len,
            Producer::Packed(bytes) => bytes.len() as u64,
            _ => self.bytes,
        })
    }

    pub(super) fn past_samples(&self, plane: Plane, first: u32) -> bool {
        let missing = |b: &Self| {
            b.reader.with_dependent(|_, r| {
                matches!(
                    r,
                    Producer::Direct(_) | Producer::Packed(_) | Producer::Finished
                )
            }) && u64::from(first).saturating_mul(b.stride as u64) >= b.stored_bytes()
        };
        match (plane, &self.mask) {
            (Plane::Mask, Some(mask)) => missing(mask),
            (Plane::Image, Some(mask)) if self.layout.mask.is_none() => {
                missing(self) && missing(mask)
            }
            _ => missing(self),
        }
    }

    pub(super) fn band_rows(&self) -> usize {
        (BAND_BYTES / self.stride).clamp(1, 64)
    }
}
