//! Images whose samples are read incrementally for each request.

use super::index::{Checkpointer, Checkpoints};
use super::rows::{
    Break, MaskRows, MaskSource, RawRow, RawRows, RowsHeld, piece_texels, rows_held,
};
use super::{
    DecodeWork, ImageLayout, PixelFormat, Plane, PlaneLayout, Stop, interleave, premultiply,
};
use crate::color::{ColorComponents, ColorSpace, ToLuma, ToRgb};
use crate::interpret::state::ActiveTransferFunction;
use crate::x_object::area::Area;
use crate::x_object::decode::{
    MaskPath, SampleFormat, SampleRows, direct_invert, sample_format, sample_lut, unpack_packed,
    unpremultiply_samples,
};
use crate::x_object::image::{ImageKind, ImageXObject};
use core::ops::Range;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::{MASK, MATTE, SMASK, SMASK_IN_DATA};
use hayro_syntax::object::stream::OwnedStream;
use hayro_syntax::work::Meter;
use smallvec::SmallVec;

/// An image read incrementally.
pub(super) struct Streamed<'a> {
    obj: ImageXObject<'a>,
    colour: Colour,
    alpha: Alpha<'a>,
    /// The rows of the colour: those whole decoding keeps where a mask's grid depends
    /// on them, else those declared.
    height: u32,
    /// `obj`'s stream, owned, which the decoder states saved in `checkpoints` keep.
    stream: &'a OwnedStream,
    checkpoints: &'a Checkpoints,
    /// The decoding opening the image did.
    pub(super) opened: DecodeWork,
}

/// How stored samples become the texels a request returns: colour, or a stencil
/// mask's alpha.
pub(super) struct Colour {
    samples: Samples,
    output: Output,
    /// Components per stored sample.
    components: usize,
}

/// How stored samples become a byte per component. Whole decoding takes samples
/// the data does not hold whole as stored `fill`.
enum Samples {
    /// A byte each, used as stored or inverted.
    Bytes { invert: bool, fill: u8 },
    /// Fewer than eight bits each, rows starting on a byte: `lut[c][v]` is what
    /// stored value `v` of component `c` becomes.
    Packed {
        bits: u8,
        lut: Vec<[u8; 256]>,
        fill: u8,
        /// For one component of bits that divide a byte, what the samples of each
        /// stored byte become, in its first `8 / bits` bytes.
        bytes: Option<Box<[[u8; 8]; 256]>>,
    },
}

impl Samples {
    fn packed(bits: u8, lut: Vec<[u8; 256]>, fill: u8) -> Self {
        let bytes = (lut.len() == 1 && 8 % bits == 0).then(|| {
            let mask = (1_u8 << bits) - 1;
            let mut table = Box::new([[0; 8]; 256]);
            for (byte, samples) in table.iter_mut().enumerate() {
                for (k, sample) in samples.iter_mut().take(usize::from(8 / bits)).enumerate() {
                    let value = (byte as u8 >> (8 - bits * (k as u8 + 1))) & mask;
                    *sample = lut[0][usize::from(value)];
                }
            }
            table
        });
        Self::Packed {
            bits,
            lut,
            fill,
            bytes,
        }
    }
}

/// What the bytes of samples are.
enum Output {
    Colour {
        color_space: ColorSpace,
        /// Whether the colour comes out as luma, not RGB.
        luma: bool,
        transfer_function: Option<ActiveTransferFunction>,
    },
    /// A stencil mask's alpha, the paint drawn through it.
    Alpha,
}

impl Colour {
    /// The conversion for `obj`'s stored samples, if they are `DeviceGray`,
    /// `DeviceRGB` or `Indexed` samples of at most eight bits (of eight, used as
    /// stored or inverted), or a stencil mask's bits or bytes used as stored or
    /// inverted.
    pub(super) fn new(obj: &ImageXObject<'_>) -> Option<Self> {
        Self::of(obj, sample_format(obj, None))
    }

    /// [`Self::new`], for samples of `format` (which a filter may set).
    pub(super) fn of(obj: &ImageXObject<'_>, format: SampleFormat) -> Option<Self> {
        let SampleFormat {
            color_space,
            bits_per_component,
            decode_arr,
        } = format;
        if obj.kind == ImageKind::StencilMask {
            let samples = match MaskPath::new(&color_space, bits_per_component, &decode_arr, true) {
                MaskPath::Bilevel { invert } => {
                    // Bits past the data are transparent, as whole decoding pads
                    // the alpha.
                    let (transparent, opaque) = if invert { (1, 0) } else { (0, 1) };
                    let mut lut = [0; 256];
                    lut[opaque] = 255;
                    Samples::packed(1, vec![lut], transparent as u8)
                }
                // Bytes past the data are transparent too.
                MaskPath::Bytes { invert } => Samples::Bytes {
                    invert,
                    fill: if invert { 255 } else { 0 },
                },
                MaskPath::Decoded => return None,
            };
            return Some(Self {
                samples,
                output: Output::Alpha,
                components: 1,
            });
        }
        if !(color_space.is_device_gray_or_rgb() || color_space.is_indexed()) {
            return None;
        }
        let samples = match bits_per_component {
            8 => Samples::Bytes {
                invert: direct_invert(&color_space, bits_per_component, &decode_arr)?,
                fill: 0,
            },
            bits => Samples::packed(bits, sample_lut(&color_space, bits, &decode_arr)?, 0),
        };
        let luma = obj
            .transfer_function
            .as_ref()
            .is_none_or(|t| matches!(t, ActiveTransferFunction::Single(_)))
            && color_space.to_luma(&mut []).is_some();
        Some(Self {
            samples,
            components: color_space.num_components() as usize,
            output: Output::Colour {
                color_space,
                luma,
                transfer_function: obj.transfer_function.clone(),
            },
        })
    }

    pub(super) fn components(&self) -> usize {
        self.components
    }

    /// Whether the samples are 8-bit `DeviceGray` or `DeviceRGB`.
    pub(super) fn device_bytes(&self) -> bool {
        matches!(self.samples, Samples::Bytes { .. })
            && matches!(&self.output, Output::Colour { color_space, .. } if color_space.is_device_gray_or_rgb())
    }

    pub(super) fn is_stencil(&self) -> bool {
        matches!(self.output, Output::Alpha)
    }

    /// The colour space of the samples; `None` for a stencil mask.
    fn color_space(&self) -> Option<&ColorSpace> {
        match &self.output {
            Output::Colour { color_space, .. } => Some(color_space),
            Output::Alpha => None,
        }
    }

    /// Bits per stored component.
    fn bits(&self) -> usize {
        match self.samples {
            Samples::Bytes { .. } => 8,
            Samples::Packed { bits, .. } => usize::from(bits),
        }
    }

    /// The stored bytes `texels` texels take, from the start of a row or from a
    /// texel the multiple of [`Self::align`] it is.
    fn bytes(&self, texels: usize) -> usize {
        (texels * self.components * self.bits()).div_ceil(8)
    }

    /// The texels a read of the samples starts at a multiple of: whole bytes.
    fn align(&self) -> usize {
        match self.samples {
            Samples::Bytes { .. } => 1,
            Samples::Packed { .. } => 8,
        }
    }

    /// How `obj`'s samples lie in rows of `height`.
    fn rows(&self, obj: &ImageXObject<'_>, height: u32) -> SampleRows {
        SampleRows::new(obj.width(), self.components, self.bits() as u8, height)
    }

    /// The stored value of samples the data does not hold whole.
    fn fill(&self) -> u8 {
        match self.samples {
            Samples::Bytes { fill, .. } | Samples::Packed { fill, .. } => fill,
        }
    }

    /// The samples `bytes` starts with, one byte each into `out` (as many as it
    /// holds): as stored with `stored`, else what they become. Those the bytes do not
    /// hold whole are [`Self::fill`].
    fn unpack(&self, bytes: &[u8], out: &mut [u8], stored: bool) {
        if let Samples::Packed {
            bits,
            bytes: Some(table),
            ..
        } = &self.samples
            && !stored
            && bytes.len() * usize::from(8 / bits) >= out.len()
        {
            let per_byte = usize::from(8 / bits);
            let whole = out.len() / per_byte;
            let mut pieces = out.chunks_exact_mut(per_byte);
            for (samples, &byte) in (&mut pieces).zip(bytes) {
                samples.copy_from_slice(&table[byte as usize][..per_byte]);
            }
            let rest = pieces.into_remainder();
            if let Some(&byte) = bytes.get(whole) {
                let len = rest.len();
                rest.copy_from_slice(&table[byte as usize][..len]);
            }
            return;
        }
        unpack_packed(self.bits() as u8, bytes, out, self.fill());
        if !stored {
            self.map(out);
        }
    }

    /// Stored values of packed samples, one byte each, into what they become.
    fn map(&self, samples: &mut [u8]) {
        let Samples::Packed { lut, .. } = &self.samples else {
            return;
        };
        if let [lut] = &lut[..] {
            samples.iter_mut().for_each(|v| *v = lut[*v as usize]);
            return;
        }
        for texel in samples.chunks_exact_mut(self.components) {
            for (v, lut) in texel.iter_mut().zip(lut) {
                *v = lut[*v as usize];
            }
        }
    }

    /// The plane of `obj`'s colour of `height` rows, with alpha or not.
    pub(super) fn plane(&self, obj: &ImageXObject<'_>, height: u32, alpha: bool) -> PlaneLayout {
        PlaneLayout {
            width: obj.width(),
            height,
            to_image: (1.0, 1.0),
            interpolate: obj.interpolate,
            format: self.format(alpha),
        }
    }

    /// What requests return: colour with alpha or not, or a stencil mask's alpha.
    pub(super) fn format(&self, alpha: bool) -> PixelFormat {
        if self.is_stencil() {
            PixelFormat::Alpha
        } else {
            PixelFormat::colour(self.channels(), alpha)
        }
    }

    /// Bytes per texel of the colour returned.
    pub(super) fn channels(&self) -> usize {
        match &self.output {
            Output::Colour { luma: false, .. } => 3,
            _ => 1,
        }
    }

    /// The colour of `samples` (one byte each, stored bytes or what packed samples
    /// become), converted where they are when the conversion keeps their size, else
    /// into `rgb`.
    pub(super) fn convert<'s>(
        &self,
        samples: &'s mut [u8],
        rgb: &'s mut Vec<u8>,
    ) -> Result<&'s mut [u8], Stop> {
        if let Samples::Bytes { invert: true, .. } = self.samples {
            samples.iter_mut().for_each(|v| *v = 255 - *v);
        }
        let Output::Colour {
            color_space,
            luma,
            transfer_function,
        } = &self.output
        else {
            return Ok(samples);
        };
        let colour = if *luma {
            color_space.to_luma(samples);
            samples
        } else if color_space.convert_in_place(samples).is_some() {
            samples
        } else {
            rgb.clear();
            rgb.resize(samples.len() / self.components * 3, 0);
            color_space.convert(samples, rgb).ok_or(Stop::Failed)?;
            rgb.as_mut_slice()
        };
        if let Some(t) = transfer_function {
            t.apply_to(colour);
        }
        Ok(colour)
    }
}

/// Where an image's alpha comes from (`decode_alpha`, in its order of precedence).
enum Alpha<'a> {
    None,
    /// A soft mask or mask stream; with `/Matte`, the colour is premultiplied
    /// against it, which is undone row by row.
    Mask {
        mask: MaskSource<'a>,
        matte: Option<[u8; 3]>,
        same_grid: bool,
    },
    /// A colour-key mask: samples within these ranges are transparent.
    ColourKey(SmallVec<[u16; 4]>),
}

impl<'a> Alpha<'a> {
    /// The alpha `decode_alpha` gives `obj`, and the rows of the colour: where there
    /// is a mask stream (whose grid, and whether `/Matte` applies, depend on them),
    /// those whole decoding keeps, else those declared. The rows of the colour's data
    /// and of the mask's are counted into `rows` once, adding to `work`. Fails where
    /// whole decoding keeps no row of the colour.
    fn new(
        obj: &ImageXObject<'a>,
        colour: &Colour,
        rows: &HeldRows,
        work: &mut DecodeWork,
        meter: &Meter<'_>,
    ) -> Result<(Self, u32), Stop> {
        let dict = obj.stream.dict();
        let smask = dict.get::<Stream<'_>>(SMASK);
        let matte = smask
            .as_ref()
            .and_then(|s| s.dict().get::<ColorComponents>(MATTE))
            .filter(|m| m.len() == colour.components);
        let in_data = dict.get::<u8>(SMASK_IN_DATA) == Some(1);
        // Only JPX data holds alpha; for other data there is none. `/Matte` takes
        // precedence when the soft mask has the image's size.
        let stream = if matte.is_some() || !in_data {
            smask.or_else(|| dict.get::<Stream<'_>>(MASK))
        } else {
            None
        };
        let Some(stream) = stream else {
            let alpha = match dict.get::<SmallVec<[u16; 4]>>(MASK) {
                Some(key) if !in_data => Alpha::ColourKey(key),
                _ => Alpha::None,
            };
            return Ok((alpha, obj.height()));
        };
        let height = match rows.colour.get() {
            Some(&held) => held,
            None => {
                let held = rows_held(&obj.stream, colour.rows(obj, obj.height()), work, meter)?;
                *rows.colour.get_or_init(|| held)
            }
        };
        let height = height.filter(|&h| h > 0).ok_or(Stop::Failed)?;
        let Some(mask) = MaskSource::new(&stream, obj, &rows.mask, work, meter)? else {
            return Ok((Alpha::None, height));
        };
        let size = (obj.width(), height);
        let matte = match matte {
            Some(m) if mask.size() == size => {
                let mut rgb = [0; 3];
                if let Some(cs) = colour.color_space() {
                    cs.convert_values(&m, &mut rgb);
                }
                Some(rgb)
            }
            _ if in_data => return Ok((Alpha::None, height)),
            _ => None,
        };
        let alpha = Alpha::Mask {
            same_grid: mask.size() == size && mask.interpolate() == obj.interpolate,
            mask,
            matte,
        };
        Ok((alpha, height))
    }

    /// Whether the alpha comes with the colour, on its grid.
    fn on_image_grid(&self) -> bool {
        matches!(
            self,
            Alpha::ColourKey(_)
                | Alpha::Mask {
                    same_grid: true,
                    ..
                }
        )
    }
}

/// The rows of an image's data and of its mask stream's data that whole decoding
/// keeps, counted when an asset is first opened, where needed.
#[derive(Default)]
pub(super) struct HeldRows {
    colour: RowsHeld,
    mask: RowsHeld,
}

impl<'a> Streamed<'a> {
    /// `obj` read incrementally, if its samples allow; the rows of its data and its
    /// mask's are counted into `rows` once, where needed.
    pub(super) fn new(
        obj: &ImageXObject<'a>,
        stream: &'a OwnedStream,
        checkpoints: &'a Checkpoints,
        rows: &HeldRows,
        meter: &Meter<'_>,
    ) -> Result<Option<Self>, Stop> {
        if !obj.stream.can_read_incrementally() {
            return Ok(None);
        }
        let Some(colour) = Colour::new(obj) else {
            return Ok(None);
        };
        let mut opened = DecodeWork::default();
        // A stencil mask is alpha alone, as whole decoding reads it.
        let (alpha, height) = if colour.is_stencil() {
            (Alpha::None, obj.height())
        } else {
            Alpha::new(obj, &colour, rows, &mut opened, meter)?
        };
        Ok(Some(Self {
            obj: obj.clone(),
            colour,
            alpha,
            height,
            stream,
            checkpoints,
            opened,
        }))
    }

    /// How the colour's samples lie in its rows.
    fn rows(&self) -> SampleRows {
        self.colour.rows(&self.obj, self.height)
    }

    pub(super) fn layout(&self) -> ImageLayout {
        let (w, h) = (self.obj.width(), self.height);
        let mask = match &self.alpha {
            Alpha::Mask {
                mask,
                same_grid: false,
                ..
            } => {
                let (mw, mh) = mask.size();
                Some(PlaneLayout {
                    width: mw,
                    height: mh,
                    to_image: (w as f64 / mw as f64, h as f64 / mh as f64),
                    interpolate: mask.interpolate(),
                    format: PixelFormat::Alpha,
                })
            }
            _ => None,
        };
        ImageLayout {
            image: self
                .colour
                .plane(&self.obj, self.height, self.alpha.on_image_grid()),
            mask,
        }
    }

    /// Reads `plane` into `area`. A reader that restarts (its data failed to decode
    /// and it reads the data again from the start, another way) starts the pass over,
    /// with the other readers rewound.
    pub(super) fn run(
        &self,
        plane: Plane,
        area: &mut Area<'_>,
        meter: &Meter<'_>,
    ) -> Result<DecodeWork, Stop> {
        match plane {
            Plane::Image => {
                let mut pass = ImagePass::new(self, area, meter)?;
                repeat_on_restart(area, |area| pass.run(area))?;
                Ok(pass.work())
            }
            Plane::Mask => {
                let Alpha::Mask { mask, .. } = &self.alpha else {
                    return Err(Stop::Failed);
                };
                let mut pass = MaskPass::new(mask, area, meter)?;
                repeat_on_restart(area, |area| pass.run(area))?;
                Ok(pass.work())
            }
        }
    }
}

/// Runs `pass` until it ends other than by a restart, with `area` emptied before
/// each repeat.
fn repeat_on_restart(
    area: &mut Area<'_>,
    mut pass: impl FnMut(&mut Area<'_>) -> Result<(), Break>,
) -> Result<(), Stop> {
    loop {
        match pass(area) {
            Ok(()) => return Ok(()),
            Err(Break::Restart) => area.reset()?,
            Err(Break::Stop(stop)) => return Err(stop),
        }
    }
}

/// A mask read in lockstep with the colour: on its grid, or with `/Matte`. It has
/// the colour's rows.
struct Lockstep<'a> {
    rows: MaskRows<'a>,
    matte: Option<[u8; 3]>,
    same_grid: bool,
    /// The alpha of the piece being read.
    alpha: Vec<u8>,
}

/// One request's reading of the colour plane.
struct ImagePass<'s, 'a> {
    image: &'s Streamed<'a>,
    raw: RawRows<'s>,
    mask: Option<Lockstep<'s>>,
    /// Saves decoder states along the rows; only without a mask read alongside, whose
    /// reader would need its own.
    checkpointer: Option<Checkpointer<'s>>,
    /// The row `raw` continues at in the first pass, resumed from a saved state.
    first_row: u32,
    /// The stored samples of the piece being read, a byte each.
    samples: Vec<u8>,
    /// The piece's stored bytes, when samples are packed.
    packed: Vec<u8>,
    /// The texel reads of a row start at: the first a request reads, or the one
    /// before it that starts a stored byte.
    start: usize,
    /// Converted colour, when it does not fit the stored samples.
    rgb: Vec<u8>,
    /// Colour with alpha, premultiplied.
    with_alpha: Vec<u8>,
    key_alpha: Vec<u8>,
    decoded: u32,
}

/// How a row read of the colour ended.
enum RowEnd {
    Full,
    /// The data ended within it, and it is kept, padded.
    Last,
    /// The data ended before or within it, and it is dropped.
    Missing,
}

impl<'s, 'a> ImagePass<'s, 'a> {
    fn new(image: &'s Streamed<'a>, area: &Area<'_>, meter: &'s Meter<'s>) -> Result<Self, Stop> {
        let h = image.height;
        let colour = &image.colour;
        let n = colour.components();
        let row_bytes = image.rows().row_bytes();
        let mut raw = RawRows::new(&image.obj.stream, row_bytes, meter).ok_or(Stop::Failed)?;
        let columns = area.columns();
        let start = columns.start as usize / colour.align() * colour.align();
        let piece = piece_texels(columns.end as usize - start, n);
        let mask = match &image.alpha {
            Alpha::Mask {
                mask,
                matte,
                same_grid,
            } if *same_grid || matte.is_some() => Some(Lockstep {
                rows: mask.rows(meter).ok_or(Stop::Failed)?,
                matte: *matte,
                same_grid: *same_grid,
                alpha: vec![0; piece],
            }),
            _ => None,
        };
        let resumed = match mask {
            Some(_) => None,
            None => image.checkpoints.checkpointer(
                image.stream,
                &mut raw,
                row_bytes,
                h,
                area.rows().start,
            )?,
        };
        let (checkpointer, first_row) =
            resumed.map_or((None, 0), |(c, first_row)| (Some(c), first_row));
        let packed = if colour.align() > 1 {
            vec![0; colour.bytes(piece)]
        } else {
            Vec::new()
        };
        Ok(Self {
            image,
            raw,
            mask,
            checkpointer,
            first_row,
            samples: vec![0; piece * n],
            packed,
            start,
            rgb: Vec::new(),
            with_alpha: Vec::with_capacity(piece * 4),
            key_alpha: Vec::new(),
            decoded: 0,
        })
    }

    fn work(&self) -> DecodeWork {
        let mask = self.mask.as_ref().map_or(0, |m| m.rows.inflated_bytes());
        DecodeWork {
            rows_decoded: u64::from(self.decoded),
            bytes_inflated: self.raw.reader.inflated_bytes() + mask,
        }
    }

    /// Reads the rows from the first to the last `area` reads, pushing those it reads.
    fn run(&mut self, area: &mut Area<'_>) -> Result<(), Break> {
        let wanted = area.rows();
        let first = std::mem::take(&mut self.first_row);
        for y in first..wanted.end.min(self.image.height) {
            let end = if y >= wanted.start {
                self.read_row(area)?
            } else {
                self.skip_row()?
            };
            if matches!(end, RowEnd::Missing) {
                // Data that holds no row fails, as decoding it whole does.
                if y == 0 {
                    return Err(Stop::Failed.into());
                }
                break;
            }
            self.decoded += 1;
            if matches!(end, RowEnd::Last) {
                break;
            }
            if let Some(c) = &self.checkpointer {
                c.after_row(y + 1, &self.raw.reader)?;
            }
        }
        Ok(())
    }

    /// Why reading the colour stopped. After its reader restarted, the mask starts
    /// over too, and the states saved are dropped (they decode the data the way that
    /// failed).
    fn colour_failed(&mut self, e: Break) -> Break {
        if matches!(e, Break::Restart) {
            if let Some(m) = &mut self.mask {
                m.rows.rewind(true);
            }
            if let Some(c) = &mut self.checkpointer
                && let Err(stop) = c.restarted()
            {
                return stop.into();
            }
        }
        e
    }

    /// Why reading the mask stopped. After its reader restarted, the colour starts
    /// over too.
    fn mask_failed(&mut self, e: Break) -> Break {
        if matches!(e, Break::Restart) {
            self.raw.rewind();
            if let Some(m) = &mut self.mask {
                m.rows.rewind(false);
            }
        }
        e
    }

    /// Reads past the next row, and the mask's.
    fn skip_row(&mut self) -> Result<RowEnd, Break> {
        let row = match self.raw.skip_row() {
            Ok(row) => row,
            Err(e) => return Err(self.colour_failed(e)),
        };
        if let Some(m) = &mut self.mask
            && let Err(e) = m.rows.skip_row()
        {
            return Err(self.mask_failed(e));
        }
        Ok(self.row_end(row))
    }

    /// Whether a row read through, as `row`, is kept.
    fn row_end(&self, row: RawRow) -> RowEnd {
        match row {
            RawRow::Full => RowEnd::Full,
            RawRow::Partial if self.keeps_row() => RowEnd::Last,
            _ => RowEnd::Missing,
        }
    }

    /// Whether the row being read is kept if the data ends where it is read to.
    fn keeps_row(&self) -> bool {
        self.image.rows().keeps_row_cut_at(self.raw.total)
    }

    /// Reads the next row, converting and pushing the columns `area` reads piece by
    /// piece. A row the data ends within is padded with the samples whole decoding
    /// pads it with, those past the mask's data pushed at once, or dropped.
    fn read_row(&mut self, area: &mut Area<'_>) -> Result<RowEnd, Break> {
        let columns = area.columns();
        let n = self.image.colour.components();
        let (c0, c1) = (columns.start as usize, columns.end as usize);
        let start = self.start;
        let piece = self.samples.len() / n;
        self.raw.begin();
        if let Some(m) = &mut self.mask {
            m.rows.begin_row();
        }
        let mut held = match self.raw.skip(self.image.colour.bytes(start)) {
            Ok(held) => held,
            Err(e) => return Err(self.colour_failed(e)),
        };
        if !held && !self.keeps_row() {
            return Ok(RowEnd::Missing);
        }
        // Whether the mask's row holds data from `col` on.
        let mut mask_held = self.mask.is_some();
        let mut col = start;
        while col < c1 {
            // The texels before `c0` a read starts at are not pushed.
            let first = col.max(c0);
            if !held && !mask_held {
                let texel = self.padding()?;
                area.push_rest(first as u32, texel)?;
                break;
            }
            let len = piece.min(c1 - col);
            let (got, wanted) = match self.read_samples(len, held) {
                Ok(read) => read,
                Err(e) => return Err(self.colour_failed(e)),
            };
            if got < wanted {
                if held && !self.keeps_row() {
                    // Only the first row of the data can be too short to keep (a later
                    // row is padded with at most the samples before it), and data
                    // that holds no row fails.
                    return Ok(RowEnd::Missing);
                }
                held = false;
                self.pad_samples(len, got);
            }
            if let Some(m) = &mut self.mask {
                match m.rows.alpha(col..col + len, &mut m.alpha[..len]) {
                    Ok(present) => mask_held = present == len,
                    Err(e) => return Err(self.mask_failed(e)),
                }
            }
            let texels = self.texels(len)?;
            let skipped = texels.len() / len * (first - col);
            area.push_texels(first as u32, &texels[skipped..])?;
            col += len;
        }
        let row = if held {
            match self.raw.finish() {
                Ok(row) => row,
                Err(e) => return Err(self.colour_failed(e)),
            }
        } else {
            RawRow::Partial
        };
        if let Some(m) = &mut self.mask
            && let Err(e) = m.rows.end_row()
        {
            return Err(self.mask_failed(e));
        }
        let end = self.row_end(row);
        if !matches!(end, RowEnd::Missing) {
            area.end_row()?;
        }
        Ok(end)
    }

    /// Reads the stored bytes of the next `len` texels, if the data still holds the
    /// row (`held`): the bytes read and those wanted. Samples of whole bytes read are
    /// unpacked.
    fn read_samples(&mut self, len: usize, held: bool) -> Result<(usize, usize), Break> {
        let n = self.image.colour.components();
        let wanted = self.image.colour.bytes(len);
        if !held {
            return Ok((0, wanted));
        }
        if self.packed.is_empty() {
            let got = self.raw.read(&mut self.samples[..len * n])?;
            return Ok((got, wanted));
        }
        let got = self.raw.read(&mut self.packed[..wanted])?;
        if got == wanted {
            let stored = self.stored();
            self.image
                .colour
                .unpack(&self.packed[..wanted], &mut self.samples[..len * n], stored);
        }
        Ok((got, wanted))
    }

    /// Whether packed samples are unpacked as stored, for a colour key to compare,
    /// and mapped after that.
    fn stored(&self) -> bool {
        matches!(self.image.alpha, Alpha::ColourKey(_))
    }

    /// Pads the samples of the next `len` texels past the `got` stored bytes read.
    fn pad_samples(&mut self, len: usize, got: usize) {
        let stored = self.stored();
        let colour = &self.image.colour;
        let n = colour.components();
        if self.packed.is_empty() {
            self.samples[got..len * n].fill(colour.fill());
            return;
        }
        colour.unpack(&self.packed[..got], &mut self.samples[..len * n], stored);
    }

    /// The texel to push for padded samples the mask's data does not reach either.
    fn padding(&mut self) -> Result<&[u8], Stop> {
        let stored = self.stored();
        let colour = &self.image.colour;
        let n = colour.components();
        colour.unpack(&[], &mut self.samples[..n], stored);
        if let Some(m) = &mut self.mask {
            m.alpha[0] = 0;
        }
        self.texels(1)
    }

    /// The texels to push of the first `len` samples read, with their alpha read.
    fn texels(&mut self, len: usize) -> Result<&[u8], Stop> {
        let n = self.image.colour.components();
        let samples = &mut self.samples[..len * n];
        if let Alpha::ColourKey(key) = &self.image.alpha {
            colour_key_alpha(samples, n, key, &mut self.key_alpha);
            self.image.colour.map(samples);
        }
        let colour = self.image.colour.convert(samples, &mut self.rgb)?;
        let channels = self.image.colour.channels();
        let alpha = match &self.mask {
            Some(m) => {
                let alpha = &m.alpha[..len];
                if let Some(matte) = &m.matte {
                    unpremultiply_samples(colour, channels, alpha, matte);
                }
                m.same_grid.then_some(alpha)
            }
            None => matches!(self.image.alpha, Alpha::ColourKey(_)).then_some(&self.key_alpha[..]),
        };
        Ok(match alpha {
            Some(alpha) => {
                premultiply(colour, channels, alpha);
                interleave(colour, channels, alpha, &mut self.with_alpha);
                &self.with_alpha
            }
            None => colour,
        })
    }
}

/// Alpha for each pixel of `samples`: transparent where every component lies within
/// its colour-key range.
fn colour_key_alpha(samples: &[u8], components: usize, key: &[u16], out: &mut Vec<u8>) {
    out.clear();
    out.extend(samples.chunks_exact(components).map(|pixel| {
        let outside = pixel
            .iter()
            .zip(key.as_chunks::<2>().0)
            .any(|(&c, r)| u16::from(c) > r[1] || u16::from(c) < r[0]);
        if outside { 255 } else { 0 }
    }));
}

/// One request's reading of a mask on its own grid.
struct MaskPass<'a> {
    rows: MaskRows<'a>,
    /// The alpha of the piece being read.
    alpha: Vec<u8>,
    height: u32,
    decoded: u32,
}

impl<'a> MaskPass<'a> {
    fn new(mask: &'a MaskSource<'a>, area: &Area<'_>, meter: &'a Meter<'a>) -> Result<Self, Stop> {
        let (_, h) = mask.size();
        Ok(Self {
            rows: mask.rows(meter).ok_or(Stop::Failed)?,
            alpha: vec![0; piece_texels(area.columns().len(), 1)],
            height: h,
            decoded: 0,
        })
    }

    fn work(&self) -> DecodeWork {
        DecodeWork {
            rows_decoded: u64::from(self.decoded),
            bytes_inflated: self.rows.inflated_bytes(),
        }
    }

    /// Moves past the rows before those `area` reads, then reads and pushes those.
    fn run(&mut self, area: &mut Area<'_>) -> Result<(), Break> {
        let (wanted, columns) = (area.rows(), area.columns());
        let (c0, c1) = (columns.start as usize, columns.end as usize);
        let end = wanted.end.min(self.height);
        let first = wanted.start.min(end);
        self.rows
            .skip_rows(first)
            .and_then(|()| (first..end).try_for_each(|_| self.read_row(area, c0..c1)))
            .inspect_err(|e| {
                if matches!(e, Break::Restart) {
                    self.rows.rewind(false);
                }
            })?;
        self.decoded += end;
        Ok(())
    }

    /// Reads the next row, pushing `columns` of it.
    fn read_row(&mut self, area: &mut Area<'_>, columns: Range<usize>) -> Result<(), Break> {
        self.rows.begin_row();
        let mut col = columns.start;
        while col < columns.end {
            let len = self.alpha.len().min(columns.end - col);
            let alpha = &mut self.alpha[..len];
            let present = self.rows.alpha(col..col + len, alpha)?;
            area.push_texels(col as u32, &alpha[..present])?;
            if present < len {
                // The rest of a last row cut short is transparent.
                area.push_rest((col + present) as u32, &[0])?;
                break;
            }
            col += len;
        }
        self.rows.end_row()?;
        area.end_row()?;
        Ok(())
    }
}
