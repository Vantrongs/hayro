use super::*;
use crate::x_object::decode::{convert_band, premultiply, unblend_matte};
use hayro_syntax::Filter;
use hayro_syntax::object::stream::{
    DecodedReader, ImageData as Metadata, ImageDecodeParams, JpegRows,
};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::AtomicBool;
use tempfile::{SpooledTempFile, spooled_tempfile};

const BAND_BYTES: usize = 128 * 1024;

enum Spool {
    File(SpooledTempFile),
    Window { base: u64, data: Vec<u8> },
}

impl Spool {
    fn append(&mut self, offset: u64, start: u64, end: u64, chunk: &[u8]) {
        if let Self::File(file) = self {
            if file
                .seek(SeekFrom::Start(offset))
                .and_then(|_| file.write_all(chunk))
                .is_ok()
            {
                return;
            }
            *self = Self::Window {
                base: offset,
                data: Vec::new(),
            };
        }
        if let Self::Window { base, data } = self {
            let discard = start.saturating_sub(*base).min(data.len() as u64) as usize;
            data.drain(..discard);
            *base += discard as u64;
            let skip = start.saturating_sub(offset).min(chunk.len() as u64) as usize;
            *base += skip as u64;
            let needed = data.len() + chunk.len() - skip;
            if needed > data.capacity() {
                let capacity = data
                    .capacity()
                    .saturating_mul(2)
                    .min((end - *base) as usize);
                data.reserve_exact(capacity.max(needed) - data.len());
            }
            data.extend_from_slice(&chunk[skip..]);
        }
    }

    fn read(&mut self, offset: u64, target: &mut [u8]) -> Result<(), RequestError> {
        match self {
            Self::File(file) => file
                .seek(SeekFrom::Start(offset))
                .and_then(|_| file.read_exact(target))
                .map_err(|_| RequestError::Failed),
            Self::Window { base, data } => {
                let at = usize::try_from(offset.checked_sub(*base).ok_or(RequestError::Failed)?)
                    .map_err(|_| RequestError::Failed)?;
                target.copy_from_slice(
                    data.get(at..)
                        .and_then(|s| s.get(..target.len()))
                        .ok_or(RequestError::Failed)?,
                );
                Ok(())
            }
        }
    }
}

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
    decoder_scratch: usize,

    token: Arc<AtomicBool>,
    spool: Spool,
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
    pub(super) fn new(asset: &ImageAsset, token: &Arc<AtomicBool>) -> Option<Self> {
        Self::from_stream(&asset.0.samples, token.clone())
    }

    fn from_stream(obj: &Samples<OwnedStream>, token: Arc<AtomicBool>) -> Option<Self> {
        if obj.in_data != 0 {
            return None;
        }
        let mask = if obj.has_mask_stream {
            Some(Box::new(Self::from_stream(
                obj.mask.as_deref()?,
                token.clone(),
            )?))
        } else {
            None
        };
        let cs = obj.color_space.as_ref()?;
        let keyed = obj.transparent_tail(cs.num_components());
        let channels = usize::from(cs.num_components());
        let bpc = obj.bpc.unwrap_or(if obj.kind == ImageKind::StencilMask {
            1
        } else {
            8
        });
        let (reader, metadata) = Self::producer(obj, &token)?;
        let bits = metadata.as_ref().map_or(bpc, |i| i.bits_per_component);
        if !matches!(bits, 1..=16) {
            return None;
        }
        let stride = (obj.width as usize)
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
        let matte = obj
            .matte
            .as_ref()
            .filter(|values| obj.has_soft_mask && values.len() == channels)
            .map(|values| {
                let mut rgb = [0; 3];
                cs.convert_values(values, &mut rgb);
                rgb
            });
        let mut layout = ImageLayout {
            image: PlaneLayout {
                width: obj.width,
                height: obj.height,
                to_image: (1., 1.),
                interpolate: obj.interpolate,
                format: match (obj.kind, gray, keyed && mask.is_none()) {
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
            if plane.width == obj.width
                && plane.height == obj.height
                && plane.interpolate == obj.interpolate
            {
                layout.image.format = sample_format.with_alpha();
            } else {
                if matte.is_some() {
                    return None;
                }
                layout.mask = Some(PlaneLayout {
                    to_image: (
                        f64::from(obj.width) / f64::from(plane.width),
                        f64::from(obj.height) / f64::from(plane.height),
                    ),
                    format: PixelFormat::Alpha,
                    ..plane
                });
            }
        }
        let band = stride * (BAND_BYTES / stride).clamp(1, 64);
        let decoder_scratch = reader.with_dependent(|_, r| match r {
            Producer::Jpeg(_) => band,
            Producer::Filter(f) => band.min(BAND_BYTES).saturating_add(f.working_bytes()),
            _ => 0,
        });
        Some(Self {
            decoder_scratch,
            mask,
            matte,
            sample_format,
            reader,
            token,
            scratch: Vec::new(),

            spool: Spool::File(spooled_tempfile(BAND_BYTES)),
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

    fn producer(
        obj: &Samples<OwnedStream>,
        token: &Arc<AtomicBool>,
    ) -> Option<(StreamRows, Option<Metadata>)> {
        let cs = obj.color_space.as_ref()?;
        let channels = usize::from(cs.num_components());
        let params = ImageDecodeParams {
            is_indexed: cs.is_indexed(),
            num_components: Some(cs.num_components()),
            width: obj.width,
            height: obj.height,
            ..Default::default()
        };
        let mut metadata = None;
        let reader = StreamRows::try_new(obj.stream.clone(), |owned| {
            let stream = owned.get().ok_or(())?;
            let last_filter = stream.filters().into_iter().rfind(|&f| f != Filter::Crypt);
            if last_filter.is_none() {
                let mut reader = stream.decrypted_reader().map_err(|_| ())?;
                if reader.can_seek() {
                    return Ok(Producer::Direct(reader.skip(usize::MAX) as u64));
                }
            }
            if last_filter == Some(Filter::CcittFaxDecode) {
                let fax = stream.fax_image(&params, Some(token.clone())).ok_or(())?;
                let info = fax.image_data.ok_or(())?;
                if params.width != info.width || params.height != info.height || channels != 1 {
                    return Err(());
                }
                metadata = Some(info);
                return Ok(Producer::Packed(fax.data.into_owned()));
            }
            if last_filter == Some(Filter::DctDecode) {
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
        Some((reader, metadata))
    }

    pub(super) fn retained_bytes(&self) -> usize {
        let storage = match &self.spool {
            Spool::File(_) => usize::try_from(self.bytes).unwrap_or(usize::MAX),
            Spool::Window { data, .. } => data.capacity(),
        };
        let producer_bytes = self.reader.with_dependent(|_, r| match r {
            Producer::Packed(bytes) => bytes.capacity(),
            Producer::Jpeg(jpeg) => jpeg.owned_input_bytes(),
            Producer::Filter(f) => size_of_val(&**f) + f.retained_bytes(),
            _ => 0,
        });
        storage
            .saturating_add(self.scratch.capacity())
            .saturating_add(producer_bytes)
            .saturating_add(self.mask.as_ref().map_or(0, |m| m.retained_bytes()))
    }

    fn advance(
        &mut self,
        first: u32,
        end: u32,
        obj: &Samples<OwnedStream>,
    ) -> Result<(bool, u32, u64), RequestError> {
        use std::sync::atomic::Ordering;
        if self.token.load(Ordering::Relaxed) {
            return Err(RequestError::Cancelled);
        }

        let end_bytes = u64::from(end).saturating_mul(self.stride as u64);
        let start = u64::from(first) * self.stride as u64;
        if let Spool::Window { base, data } = &mut self.spool
            && start < *base
        {
            self.reader = Self::producer(obj, &self.token)
                .ok_or(RequestError::Failed)?
                .0;
            self.bytes = 0;
            self.eof = false;
            *base = 0;
            data.clear();
        }
        let read_size = (self.stride * self.band_rows())
            .min(usize::try_from(end_bytes.saturating_sub(self.bytes)).unwrap_or(usize::MAX));
        let direct = self
            .reader
            .with_dependent(|_, r| matches!(r, Producer::Direct(_) | Producer::Packed(_)));
        let mut advanced = 0;
        let mut inflated = 0;
        if !direct && self.bytes < end_bytes && !self.eof {
            let result = self.reader.with_dependent_mut(|_, r| match r {
                Producer::Jpeg(jpeg) => {
                    self.scratch.resize(read_size, 0);
                    jpeg.read(&mut self.scratch).map(|n| n * self.stride)
                }
                Producer::Filter(filter) => {
                    self.scratch.resize(read_size.min(BAND_BYTES), 0);
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
                self.spool
                    .append(self.bytes, start, end_bytes, &self.scratch[..n]);
                let previous = self.bytes;
                self.bytes += n as u64;
                advanced = (self.bytes / self.stride as u64 - previous / self.stride as u64) as u32;
                if self.bytes < end_bytes
                    || matches!(&self.spool, Spool::Window { base, .. } if start < *base)
                {
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
        obj: &Samples<OwnedStream>,
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
                            .get()
                            .ok_or(RequestError::Failed)?
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
                        self.spool.read(offset, target)?;
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
        obj: &Samples<OwnedStream>,
    ) -> Result<Band, RequestError> {
        let combined = self.mask.is_some() && self.layout.mask.is_none();
        let (ready, mut rows, mut bytes) = if plane == Plane::Image {
            self.advance(first, end, obj)?
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
            let (done, m, n) =
                mask.advance(first, end, obj.mask.as_deref().ok_or(RequestError::Failed)?)?;
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
        obj: &Samples<OwnedStream>,
    ) -> Result<Vec<u8>, RequestError> {
        if self.mask.is_none() || plane == Plane::Image && self.layout.mask.is_some() {
            return self.samples(columns, first, end, obj);
        }
        let mask_obj = obj.mask.as_deref().ok_or(RequestError::Failed)?;
        let mask = self.mask.as_mut().unwrap();
        if plane == Plane::Mask {
            return mask.samples(columns, first, end, mask_obj);
        }
        let mut alpha = mask.samples(columns, first, end, mask_obj)?;
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

    pub(super) fn scratch_layout(&self, plane: Plane) -> [Option<NativeLayout>; 2] {
        let native = |b: &Bands| {
            let finished = b
                .reader
                .with_dependent(|_, r| matches!(r, Producer::Finished));
            let scratch = if finished && matches!(b.spool, Spool::File(_)) {
                0
            } else {
                b.decoder_scratch
            };
            NativeLayout {
                bits: b.pixel_bits,
                channels: b.pixel_bits / usize::from(b.bits) + usize::from(b.keyed),
                converted: b.sample_format.channels(),
                decoder: if scratch == 0 {
                    0
                } else {
                    scratch.saturating_add(
                        BAND_BYTES.min(b.stride.saturating_mul(b.layout.image.height as usize)),
                    )
                },
                rows: self.request_rows(plane),
            }
        };
        match (plane, &self.mask) {
            (Plane::Mask, Some(m)) => [Some(native(m)), None],
            (Plane::Image, Some(m)) if self.layout.mask.is_none() => {
                [Some(native(self)), Some(native(m))]
            }
            _ => [Some(native(self)), None],
        }
    }

    pub(super) fn request_rows(&self, plane: Plane) -> usize {
        match (&self.mask, plane) {
            (Some(mask), Plane::Mask) => mask.band_rows(),
            (Some(mask), Plane::Image) if self.layout.mask.is_none() => {
                self.band_rows().min(mask.band_rows())
            }
            _ => self.band_rows(),
        }
    }

    fn band_rows(&self) -> usize {
        (BAND_BYTES / self.stride).clamp(1, 64)
    }
}

#[derive(Clone, Copy)]
pub(super) struct NativeLayout {
    bits: usize,
    channels: usize,
    converted: usize,
    decoder: usize,
    rows: usize,
}

impl NativeLayout {
    pub(super) fn bytes(self, mapped: [u32; 4]) -> usize {
        let align = 8 >> self.bits.trailing_zeros().min(3);
        let columns = mapped[2] as usize - (mapped[0] as usize / align * align);
        let rows = self.rows.min((mapped[3] - mapped[1]) as usize);
        let row = columns.saturating_mul(self.bits).div_ceil(8);
        self.decoder.saturating_add(rows.saturating_mul(
            row.saturating_add(columns.saturating_mul(self.channels + self.converted)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x_object::asset::short_rows::{image, pdf};

    #[test]
    fn failed_spill_preserves_decoder_errors() {
        let mut encoded = vec![b'0'; 3 * BAND_BYTES];
        encoded.push(b'G');
        let doc = pdf(&[("/BPC 8 /Width 2048 /Height 192 /Filter /AHx", &encoded)]);
        let asset = image(&doc).asset(None);
        let mut source = asset.open(None).unwrap();
        let bad_dir = tempfile::NamedTempFile::new().unwrap();
        source.bands.as_mut().unwrap().spool =
            Spool::File(SpooledTempFile::new_in(0, bad_dir.path()));
        let result = source.request(&ImageRequest {
            plane: Plane::Image,
            grid: (2048, 192),
            window: [0, 0, 2048, 192],
            cancel: None,
        });
        assert!(matches!(result, Err(RequestError::Failed)));
    }

    #[test]
    fn forward_and_backward_bands_survive_failed_spill() {
        let width = 2048;
        let height = 192;
        let mut encoded = Vec::new();
        for row in 0..height {
            for _ in 0..width / 128 {
                encoded.extend_from_slice(&[129, row as u8]);
            }
        }
        encoded.push(128);
        let doc = pdf(&[("/BPC 8 /Width 2048 /Height 192 /Filter /RL", &encoded)]);
        let asset = image(&doc).asset(None);
        let bad_dir = tempfile::NamedTempFile::new().unwrap();
        for fail in [false, true] {
            let mut source = asset.open(None).unwrap();
            if fail {
                source.bands.as_mut().unwrap().spool =
                    Spool::File(SpooledTempFile::new_in(BAND_BYTES, bad_dir.path()));
            }
            for (first, end) in [(0, 43), (43, 104), (128, 192), (100, 149), (0, 64)] {
                let result = source
                    .request(&ImageRequest {
                        plane: Plane::Image,
                        grid: (width, height),
                        window: [0, first, width, end],
                        cancel: None,
                    })
                    .unwrap();
                assert_eq!(result.data.len(), ((end - first) * width) as usize);
                for (row, data) in result.data.chunks_exact(width as usize).enumerate() {
                    assert!(data.iter().all(|&v| v == first as u8 + row as u8));
                }
                assert!(source.checkpoint_bytes() > 0);
            }
            if fail {
                let bands = &source.bands;
                let Spool::Window { data, .. } = &bands.as_ref().unwrap().spool else {
                    panic!("expected window after spill failure");
                };
                assert!(data.capacity() <= BAND_BYTES);
            }
        }
    }
}
