//! Images whose samples are read incrementally for each request.

use super::index::{Checkpointer, Checkpoints};
use super::rows::{DataEnds, MaskRows, MaskSource, RawRow, RawRows, piece_texels, stepped};
use super::{
    DecodeWork, ImageLayout, PixelFormat, Plane, PlaneLayout, Stop, interleave, premultiply,
};
use crate::color::{ColorComponents, ColorSpace, ToLuma, ToRgb};
use crate::interpret::state::ActiveTransferFunction;
use crate::x_object::area::Area;
use crate::x_object::decode::{
    MaskPath, SampleFormat, SampleRows, colour_key, direct_invert, image_decode_params, mask_lut,
    sample_format, sample_lut, short_rows_transparent, unpack_packed, unpremultiply_samples,
};
use crate::x_object::image::{ImageKind, ImageXObject};
use core::ops::Range;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::{MASK, MATTE, SMASK, SMASK_IN_DATA};
use hayro_syntax::object::stream::{ImageData, OwnedStream};
use hayro_syntax::work::{Cancelled, Meter};
use smallvec::SmallVec;
use std::sync::OnceLock;

/// An image read incrementally.
pub(super) struct Streamed<'a> {
    obj: ImageXObject<'a>,
    colour: Colour,
    alpha: Alpha<'a>,
    grid: Grid,
    /// Whether rows the colour's data does not hold whole are transparent
    /// (`short_rows_transparent`), not stored zero.
    transparent: bool,
    /// `obj`'s stream, owned, which the decoder states saved in `checkpoints` keep.
    stream: &'a OwnedStream,
    checkpoints: &'a Checkpoints,
    ends: &'a DataEnds,
}

/// How an image's stored samples lie, as whole decoding reads them: in rows of the
/// width and height the dictionary declares, or of those its filter reports (CCITT
/// data's `Columns`), mapped onto the declared grid.
#[derive(Clone, Copy)]
pub(super) struct Grid {
    pub(super) width: u32,
    pub(super) height: u32,
    /// The size of a sample in the declared grid, per axis.
    pub(super) to_image: (f64, f64),
}

impl Grid {
    /// The grid `obj`'s dictionary declares.
    pub(super) fn declared(obj: &ImageXObject<'_>) -> Self {
        Self {
            width: obj.width(),
            height: obj.height(),
            to_image: (1.0, 1.0),
        }
    }

    /// `obj`'s grid, where its filter reports `data` of its samples; `None` for a
    /// grid of no samples.
    fn of(obj: &ImageXObject<'_>, data: Option<&ImageData>) -> Option<Self> {
        let Some(d) = data else {
            return Some(Self::declared(obj));
        };
        (d.width > 0 && d.height > 0).then(|| Self {
            width: d.width,
            height: d.height,
            // As whole decoding maps samples of another size than the dictionary
            // declares onto the image.
            to_image: (
                f64::from(obj.width() as f32 / d.width as f32),
                f64::from(obj.height() as f32 / d.height as f32),
            ),
        })
    }
}

/// The grid and format of `obj`'s samples, as whole decoding reads them, where its
/// filters allow reading them incrementally.
pub(super) fn incremental_samples(obj: &ImageXObject<'_>) -> Option<(Grid, SampleFormat)> {
    if !obj.stream.image_reads_incrementally() {
        return None;
    }
    let data = obj.stream.ccitt_image_data(&image_decode_params(obj));
    Some((
        Grid::of(obj, data.as_ref())?,
        sample_format(obj, data.as_ref()),
    ))
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
/// the data does not hold as stored zero.
enum Samples {
    /// A byte each, used as stored or inverted.
    Bytes { invert: bool },
    /// Fewer than eight bits each, rows starting on a byte: `lut[c][v]` is what
    /// stored value `v` of component `c` becomes.
    Packed {
        bits: u8,
        lut: Vec<[u8; 256]>,
        /// For one component of bits that divide a byte, what the samples of each
        /// stored byte become, in its first `8 / bits` bytes.
        bytes: Option<Box<[[u8; 8]; 256]>>,
    },
}

impl Samples {
    fn packed(bits: u8, lut: Vec<[u8; 256]>) -> Self {
        let bytes = (lut.len() == 1 && bits < 8 && 8 % bits == 0).then(|| {
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
        Self::Packed { bits, lut, bytes }
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
    /// The conversion for `obj`'s stored samples, if they are samples of fewer than
    /// eight bits, 8-bit samples of one component or `DeviceRGB` samples used as
    /// stored or inverted, other 8-bit samples of one component, or a stencil mask's
    /// samples.
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
                    let mut lut = [0; 256];
                    lut[usize::from(!invert)] = 255;
                    Samples::packed(1, vec![lut])
                }
                MaskPath::Bytes { invert } => Samples::Bytes { invert },
                MaskPath::Decoded => Samples::packed(
                    bits_per_component,
                    vec![mask_lut(bits_per_component, &decode_arr, true)?],
                ),
            };
            return Some(Self {
                samples,
                output: Output::Alpha,
                components: 1,
            });
        }
        if color_space.num_components() == 0 {
            return None;
        }
        let one = color_space.num_components() == 1;
        let direct = one || color_space.is_device_gray_or_rgb();
        let samples = match (
            bits_per_component,
            direct_invert(&color_space, 8, &decode_arr),
        ) {
            (8, Some(invert)) if direct => Samples::Bytes { invert },
            (8, _) if !one => return None,
            // Through a table of the values a byte holds, as samples of fewer bits.
            (bits, _) => Samples::packed(bits, sample_lut(&color_space, bits, &decode_arr)?),
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

    /// How samples lie in rows of `width` and `height`.
    fn rows(&self, width: u32, height: u32) -> SampleRows {
        SampleRows::new(width, self.components, self.bits() as u8, height)
    }

    /// The samples `bytes` starts with, one byte each into `out` (as many as it
    /// holds): as stored with `stored`, else what they become. Those the bytes do not
    /// hold whole are stored zero.
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
        unpack_packed(self.bits() as u8, bytes, out, 0);
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

    /// The plane of `obj`'s colour on `grid`, with alpha or not.
    pub(super) fn plane(&self, obj: &ImageXObject<'_>, grid: &Grid, alpha: bool) -> PlaneLayout {
        PlaneLayout {
            width: grid.width,
            height: grid.height,
            to_image: grid.to_image,
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
    /// The alpha `decode_alpha` gives `obj`, a mask lying on the grid the colour
    /// declares.
    fn new(
        obj: &ImageXObject<'a>,
        colour: &Colour,
        grid: &Grid,
        meter: &Meter<'_>,
    ) -> Result<Self, Stop> {
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
            return Ok(match colour_key(obj, colour.components) {
                Some(key) if !in_data => Alpha::ColourKey(key),
                _ => Alpha::None,
            });
        };
        let Some(mask) = MaskSource::new(&stream, obj, meter)? else {
            return Ok(Alpha::None);
        };
        let size = (grid.width, grid.height);
        let matte = match matte {
            Some(m) if mask.size() == size => {
                let mut rgb = [0; 3];
                if let Some(cs) = colour.color_space() {
                    cs.convert_values(&m, &mut rgb);
                }
                Some(rgb)
            }
            _ if in_data => return Ok(Alpha::None),
            _ => None,
        };
        Ok(Alpha::Mask {
            same_grid: mask.size() == size && mask.interpolate() == obj.interpolate,
            mask,
            matte,
        })
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

impl<'a> Streamed<'a> {
    /// `obj` read incrementally, if its samples allow; where its data and its mask's
    /// end is noted in `ends`.
    pub(super) fn new(
        obj: &ImageXObject<'a>,
        stream: &'a OwnedStream,
        checkpoints: &'a Checkpoints,
        ends: &'a DataEnds,
        meter: &Meter<'_>,
    ) -> Result<Option<Self>, Stop> {
        let Some((grid, format)) = incremental_samples(obj) else {
            return Ok(None);
        };
        let Some(colour) = Colour::of(obj, format) else {
            return Ok(None);
        };
        // A stencil mask is alpha alone, as whole decoding reads it.
        let alpha = if colour.is_stencil() {
            Alpha::None
        } else {
            Alpha::new(obj, &colour, &grid, meter)?
        };
        let transparent = short_rows_transparent(obj, colour.components);
        Ok(Some(Self {
            obj: obj.clone(),
            colour,
            alpha,
            grid,
            transparent,
            stream,
            checkpoints,
            ends,
        }))
    }

    /// How the colour's samples lie in its rows.
    fn rows(&self) -> SampleRows {
        self.colour.rows(self.grid.width, self.grid.height)
    }

    /// Whether every stored byte of a colour row the data ends within counts; else
    /// only whole rows do.
    fn byte_granular(&self) -> bool {
        self.rows().byte_granular() && !self.transparent
    }

    pub(super) fn layout(&self) -> ImageLayout {
        let (w, h) = (self.grid.width, self.grid.height);
        let (sx, sy) = self.grid.to_image;
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
                    // As whole decoding lays the mask on the grid the colour declares.
                    to_image: (sx * w as f64 / mw as f64, sy * h as f64 / mh as f64),
                    interpolate: mask.interpolate(),
                    format: PixelFormat::Alpha,
                })
            }
            _ => None,
        };
        ImageLayout {
            image: self
                .colour
                .plane(&self.obj, &self.grid, self.alpha.on_image_grid()),
            mask,
        }
    }

    /// Reads `plane` into `area`.
    pub(super) fn run(
        &self,
        plane: Plane,
        area: &mut Area<'_>,
        meter: &Meter<'_>,
    ) -> Result<DecodeWork, Stop> {
        let mut work = DecodeWork::default();
        // A pass stops to be read again only on finding where the colour's or the
        // mask's data ends (`Stop::Retry`), and the next pass knows it: each pass
        // read again knows one end more, so there are at most three.
        loop {
            let known = self.ends.known();
            let result = match plane {
                Plane::Image => {
                    let mut pass = ImagePass::new(self, area, meter)?;
                    let result = pass.run(area);
                    work.add(pass.work());
                    result
                }
                Plane::Mask => {
                    let Alpha::Mask { mask, .. } = &self.alpha else {
                        return Err(Stop::Failed);
                    };
                    let mut pass = MaskPass::new(mask, &self.ends.mask, area, meter)?;
                    let result = pass.run(area);
                    work.add(pass.work());
                    result
                }
            };
            match result {
                Err(Stop::Retry) if self.ends.known() > known => area.reset()?,
                Err(Stop::Retry) => return Err(Stop::Failed),
                result => return result.map(|()| work),
            }
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
    /// The data holds it whole.
    Whole,
    /// The data ends within it, and it holds the 8-bit samples before the end,
    /// stored zero after it.
    Cut,
    /// It reads as stored zero: the data ends before it, or within it where only
    /// whole rows count.
    Zero,
    /// It and the rows after it are transparent: the data does not hold it whole,
    /// and short rows are transparent.
    Missing,
}

impl<'s, 'a> ImagePass<'s, 'a> {
    fn new(image: &'s Streamed<'a>, area: &Area<'_>, meter: &'s Meter<'s>) -> Result<Self, Stop> {
        let colour = &image.colour;
        let n = colour.components();
        let row_bytes = image.rows().row_bytes();
        let mut raw =
            RawRows::new(&image.obj, row_bytes, &image.ends.colour, meter).ok_or(Stop::Failed)?;
        let columns = area.columns();
        let start = columns.start as usize / colour.align() * colour.align();
        let piece = piece_texels(columns.end as usize - start, n);
        let mask = match &image.alpha {
            Alpha::Mask {
                mask,
                matte,
                same_grid,
            } if *same_grid || matte.is_some() => Some(Lockstep {
                rows: mask.rows(&image.ends.mask, meter).ok_or(Stop::Failed)?,
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
                image.grid.height,
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
            with_alpha: Vec::new(),
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
    fn run(&mut self, area: &mut Area<'_>) -> Result<(), Stop> {
        let wanted = area.rows();
        let mut y = std::mem::take(&mut self.first_row);
        while y < wanted.end.min(self.image.grid.height) {
            if y < wanted.start {
                let to = self.jump(y, wanted.start)?;
                if to > y {
                    y = to;
                    continue;
                }
            }
            if y < wanted.start && !self.reads(y) && !self.image.transparent {
                // The data ends before row `y`, so it and every row up to those read
                // are stored zero: only the mask is read past, up to there.
                if let Some(m) = &mut self.mask {
                    m.rows.skip_rows(wanted.start - y)?;
                }
                y = wanted.start;
                continue;
            }
            if y >= wanted.start && self.past_data(y) {
                // Every row from `y` on is stored zero, and the mask's too: added at
                // once, in time the bins bound.
                let end = wanted.end.min(self.image.grid.height);
                area.push_rows(end - y, self.padding()?)?;
                break;
            }
            let end = if y >= wanted.start {
                self.read_row(y, area)?
            } else {
                self.skip_row(y)?
            };
            match end {
                RowEnd::Missing => {
                    // Data that holds no whole row fails, as decoding it whole does.
                    if y == 0 {
                        return Err(Stop::Failed);
                    }
                    break;
                }
                RowEnd::Whole => {
                    self.decoded += 1;
                    if let Some(c) = &self.checkpointer {
                        c.after_row(y + 1, &self.raw.reader)?;
                    }
                }
                RowEnd::Cut => self.decoded += 1,
                RowEnd::Zero => {}
            }
            y += 1;
        }
        Ok(())
    }

    /// Moves past rows from `y`, up to `to`, without reading them, where the colour's
    /// reader seeks and its data is known to hold them whole (and through a mask read
    /// alongside, as it moves past rows), up to the next row due a saved state, which
    /// it saves; returns the row it got to, `y` where it moved past none.
    fn jump(&mut self, y: u32, to: u32) -> Result<u32, Stop> {
        let mut n = self.raw.seekable_rows(y, to - y);
        if let Some(c) = &self.checkpointer {
            n = n.min(c.next_due(y) - y);
        }
        if n == 0 {
            return Ok(y);
        }
        if let Some(m) = &mut self.mask {
            m.rows.skip_rows(n)?;
        }
        self.raw.seek_rows(n)?;
        if let Some(c) = &self.checkpointer {
            c.after_row(y + n, &self.raw.reader)?;
        }
        Ok(y + n)
    }

    /// Whether the bytes of row `y` are read: until the data's end is known, every
    /// row's; then those of rows it holds whole, and of a row it ends within where
    /// every byte counts.
    fn reads(&self, y: u32) -> bool {
        let known = if self.image.byte_granular() {
            self.raw.reaches_row(y)
        } else {
            self.raw.holds_row(y)
        };
        known.unwrap_or(true)
    }

    /// Whether row `y` and every row after it read as stored zero, as the rows of a
    /// mask read alongside do: the data and the mask's are known to end before them.
    fn past_data(&self, y: u32) -> bool {
        !self.reads(y)
            && !self.image.transparent
            && self.mask.as_ref().is_none_or(|m| m.rows.fills_rest())
    }

    /// How a row the data does not hold whole and that keeps no byte ends.
    fn short(&self) -> RowEnd {
        if self.image.transparent {
            RowEnd::Missing
        } else {
            RowEnd::Zero
        }
    }

    /// Reads past row `y`, and the mask's.
    fn skip_row(&mut self, y: u32) -> Result<RowEnd, Stop> {
        stepped();
        if let Some(m) = &mut self.mask {
            m.rows.skip_row()?;
        }
        if !self.reads(y) {
            return Ok(self.short());
        }
        Ok(match self.raw.skip_row()? {
            RawRow::Full => RowEnd::Whole,
            RawRow::Partial if self.image.byte_granular() => RowEnd::Cut,
            _ => self.short(),
        })
    }

    /// Reads row `y`, converting and pushing the columns `area` reads piece by piece.
    /// Samples the data does not hold are stored zero, those past the mask's data
    /// too pushed at once; where only whole rows count, a row the data ends within
    /// is all stored zero, or missing where short rows are transparent. Fails with
    /// [`Stop::Retry`] where such a row turns out short after part of it was pushed.
    fn read_row(&mut self, y: u32, area: &mut Area<'_>) -> Result<RowEnd, Stop> {
        let reads = self.reads(y);
        if !reads && self.image.transparent {
            return Ok(RowEnd::Missing);
        }
        let granular = self.image.byte_granular();
        let columns = area.columns();
        let n = self.image.colour.components();
        let (c0, c1) = (columns.start as usize, columns.end as usize);
        let start = self.start;
        let piece = self.samples.len() / n;
        self.raw.begin();
        if let Some(m) = &mut self.mask {
            m.rows.begin_row();
        }
        // Whether the data holds the samples from `col` on.
        let mut held = reads && self.raw.skip(self.image.colour.bytes(start))?;
        if reads && !held && self.image.transparent {
            return Ok(RowEnd::Missing);
        }
        let mut cut = reads && !held && granular;
        let mut pushed = false;
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
            let (got, wanted) = self.read_samples(len, held)?;
            if got < wanted {
                if held && !granular {
                    if pushed {
                        return Err(Stop::Retry);
                    }
                    if self.image.transparent {
                        return Ok(RowEnd::Missing);
                    }
                }
                cut |= held && granular;
                held = false;
                self.pad_samples(len, if granular { got } else { 0 });
            }
            if let Some(m) = &mut self.mask {
                mask_held = m.rows.alpha(col..col + len, &mut m.alpha[..len])?;
            }
            let texels = self.texels(len)?;
            let skipped = texels.len() / len * (first - col);
            area.push_texels(first as u32, &texels[skipped..])?;
            pushed = true;
            col += len;
        }
        if held {
            match self.raw.finish()? {
                RawRow::Full => {}
                _ if granular => cut = true,
                // The data ends past the columns pushed, within the row.
                _ => return Err(Stop::Retry),
            }
        }
        if let Some(m) = &mut self.mask {
            m.rows.end_row()?;
        }
        area.end_row()?;
        Ok(if cut {
            RowEnd::Cut
        } else if held {
            RowEnd::Whole
        } else {
            RowEnd::Zero
        })
    }

    /// Reads the stored bytes of the next `len` texels, if the data still holds the
    /// row (`held`): the bytes read and those wanted. Samples of whole bytes read are
    /// unpacked.
    fn read_samples(&mut self, len: usize, held: bool) -> Result<(usize, usize), Cancelled> {
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

    /// The samples of the next `len` texels, of which the first `got` stored bytes
    /// were read, the rest stored zero.
    fn pad_samples(&mut self, len: usize, got: usize) {
        let stored = self.stored();
        let colour = &self.image.colour;
        let n = colour.components();
        if self.packed.is_empty() {
            self.samples[got..len * n].fill(0);
            return;
        }
        colour.unpack(&self.packed[..got], &mut self.samples[..len * n], stored);
    }

    /// The texel to push for samples stored zero that the mask's data does not
    /// reach either.
    fn padding(&mut self) -> Result<&[u8], Stop> {
        let stored = self.stored();
        let colour = &self.image.colour;
        let n = colour.components();
        colour.unpack(&[], &mut self.samples[..n], stored);
        if let Some(m) = &mut self.mask {
            m.alpha[0] = m.rows.fill();
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
    fn new(
        mask: &'a MaskSource<'a>,
        end: &'a OnceLock<usize>,
        area: &Area<'_>,
        meter: &'a Meter<'a>,
    ) -> Result<Self, Stop> {
        let (_, h) = mask.size();
        Ok(Self {
            rows: mask.rows(end, meter).ok_or(Stop::Failed)?,
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
    fn run(&mut self, area: &mut Area<'_>) -> Result<(), Stop> {
        let (wanted, columns) = (area.rows(), area.columns());
        let (c0, c1) = (columns.start as usize, columns.end as usize);
        let end = wanted.end.min(self.height);
        let first = wanted.start.min(end);
        self.rows.skip_rows(first)?;
        for y in first..end {
            if self.rows.fills_rest() {
                // Every row from `y` on is stored zero's alpha: added at once, in time
                // the bins bound.
                area.push_rows(end - y, &[self.rows.fill()])?;
                break;
            }
            self.read_row(area, c0..c1)?;
        }
        self.decoded += end;
        Ok(())
    }

    /// Reads the next row, pushing `columns` of it; past the mask's data, stored
    /// zero's alpha at once.
    fn read_row(&mut self, area: &mut Area<'_>, columns: Range<usize>) -> Result<(), Stop> {
        self.rows.begin_row();
        if self.rows.fills_row() {
            area.push_rest(columns.start as u32, &[self.rows.fill()])?;
            self.rows.end_row()?;
            area.end_row()?;
            return Ok(());
        }
        let mut col = columns.start;
        while col < columns.end {
            let len = self.alpha.len().min(columns.end - col);
            let alpha = &mut self.alpha[..len];
            let held = self.rows.alpha(col..col + len, alpha)?;
            area.push_texels(col as u32, alpha)?;
            col += len;
            if !held && col < columns.end {
                area.push_rest(col as u32, &[self.rows.fill()])?;
                break;
            }
        }
        self.rows.end_row()?;
        area.end_row()?;
        Ok(())
    }
}
