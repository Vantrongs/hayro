//! Images whose samples are read incrementally for each request.

use super::index::{Checkpointer, Checkpoints};
use super::rows::{
    Break, MaskHeight, MaskRows, MaskSource, RawRow, RawRows, keeps_partial_row, piece_texels,
};
use super::{
    DecodeWork, ImageLayout, PixelFormat, Plane, PlaneLayout, Stop, interleave, premultiply,
};
use crate::color::{ColorComponents, ColorSpace, ToLuma, ToRgb};
use crate::interpret::state::ActiveTransferFunction;
use crate::x_object::area::Area;
use crate::x_object::decode::{SampleFormat, direct_invert, sample_format, unpremultiply_samples};
use crate::x_object::image::ImageXObject;
use core::ops::Range;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::{MASK, MATTE, SMASK, SMASK_IN_DATA};
use hayro_syntax::object::stream::StreamReader;
use hayro_syntax::work::{Cancelled, Meter};
use smallvec::SmallVec;

/// An image read incrementally.
pub(super) struct Streamed<'a> {
    obj: ImageXObject<'a>,
    colour: Colour,
    alpha: Alpha<'a>,
    /// `obj`'s stream, owned, which the decoder states saved in `checkpoints` keep.
    stream: &'a StreamReader,
    checkpoints: &'a Checkpoints,
    /// The decoding opening the image did.
    pub(super) opened: DecodeWork,
}

/// How stored 8-bit samples become the colour a request returns.
pub(super) struct Colour {
    color_space: ColorSpace,
    /// Components per stored sample.
    components: usize,
    invert: bool,
    /// Whether the colour comes out as luma, not RGB.
    luma: bool,
    transfer_function: Option<ActiveTransferFunction>,
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

impl Colour {
    /// The conversion for `obj`'s samples, if they are 8-bit `DeviceGray` or
    /// `DeviceRGB` used as stored or inverted.
    pub(super) fn new(obj: &ImageXObject<'_>) -> Option<Self> {
        let SampleFormat {
            color_space,
            bits_per_component,
            decode_arr,
        } = sample_format(obj, None);
        if bits_per_component != 8 || !color_space.is_device_gray_or_rgb() {
            return None;
        }
        let invert = direct_invert(&color_space, bits_per_component, &decode_arr)?;
        let luma = obj
            .transfer_function
            .as_ref()
            .is_none_or(|t| matches!(t, ActiveTransferFunction::Single(_)))
            && color_space.to_luma(&mut []).is_some();
        Some(Self {
            components: color_space.num_components() as usize,
            color_space,
            invert,
            luma,
            transfer_function: obj.transfer_function.clone(),
        })
    }

    pub(super) fn components(&self) -> usize {
        self.components
    }

    /// The plane of `obj`'s colour, with alpha or not.
    pub(super) fn plane(&self, obj: &ImageXObject<'_>, alpha: bool) -> PlaneLayout {
        PlaneLayout {
            width: obj.width(),
            height: obj.height(),
            to_image: (1.0, 1.0),
            interpolate: obj.interpolate,
            format: PixelFormat::colour(self.channels(), alpha),
        }
    }

    /// Bytes per texel of the colour returned.
    pub(super) fn channels(&self) -> usize {
        if self.luma { 1 } else { 3 }
    }

    /// The colour of `samples`, converted where they are when the conversion keeps
    /// their size, else into `rgb`.
    pub(super) fn convert<'s>(
        &self,
        samples: &'s mut [u8],
        rgb: &'s mut Vec<u8>,
    ) -> Result<&'s mut [u8], Stop> {
        if self.invert {
            samples.iter_mut().for_each(|v| *v = 255 - *v);
        }
        let colour = if self.luma {
            self.color_space.to_luma(samples);
            samples
        } else if self.color_space.convert_in_place(samples).is_some() {
            samples
        } else {
            rgb.clear();
            rgb.resize(samples.len() / self.components * 3, 0);
            self.color_space.convert(samples, rgb).ok_or(Stop::Failed)?;
            rgb.as_mut_slice()
        };
        if let Some(t) = &self.transfer_function {
            t.apply_to(colour);
        }
        Ok(colour)
    }
}

impl<'a> Alpha<'a> {
    /// The alpha `decode_alpha` gives `obj`, a mask stream's rows counted into
    /// `height` once, adding to `work`.
    fn new(
        obj: &ImageXObject<'a>,
        colour: &Colour,
        height: &MaskHeight,
        work: &mut DecodeWork,
        meter: &Meter<'_>,
    ) -> Result<Self, Cancelled> {
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
            return Ok(match dict.get::<SmallVec<[u16; 4]>>(MASK) {
                Some(key) if !in_data => Alpha::ColourKey(key),
                _ => Alpha::None,
            });
        };
        let Some(mask) = MaskSource::new(&stream, obj, height, work, meter)? else {
            return Ok(Alpha::None);
        };
        let size = (obj.width(), obj.height());
        let matte = match matte {
            Some(m) if mask.size() == size => {
                let mut rgb = [0; 3];
                colour.color_space.convert_values(&m, &mut rgb);
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
    /// `obj` read incrementally, if its samples allow; its mask's rows are counted
    /// into `mask_height` once.
    pub(super) fn new(
        obj: &ImageXObject<'a>,
        stream: &'a StreamReader,
        checkpoints: &'a Checkpoints,
        mask_height: &MaskHeight,
        meter: &Meter<'_>,
    ) -> Result<Option<Self>, Cancelled> {
        if !obj.stream.can_read_incrementally() {
            return Ok(None);
        }
        let Some(colour) = Colour::new(obj) else {
            return Ok(None);
        };
        let mut opened = DecodeWork::default();
        let alpha = Alpha::new(obj, &colour, mask_height, &mut opened, meter)?;
        Ok(Some(Self {
            obj: obj.clone(),
            colour,
            alpha,
            stream,
            checkpoints,
            opened,
        }))
    }

    pub(super) fn layout(&self) -> ImageLayout {
        let (w, h) = (self.obj.width(), self.obj.height());
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
            image: self.colour.plane(&self.obj, self.alpha.on_image_grid()),
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
    start: u32,
    /// The stored samples of the piece being read.
    samples: Vec<u8>,
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
        let h = image.obj.height();
        let n = image.colour.components();
        let row_bytes = image.obj.width() as usize * n;
        let mut raw = RawRows::new(&image.obj.stream, row_bytes, meter).ok_or(Stop::Failed)?;
        let columns = area.columns();
        let piece = piece_texels(columns.len(), n);
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
        let (checkpointer, start) = resumed.map_or((None, 0), |(c, start)| (Some(c), start));
        Ok(Self {
            image,
            raw,
            mask,
            checkpointer,
            start,
            samples: vec![0; piece * n],
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
        let first = std::mem::take(&mut self.start);
        for y in first..wanted.end.min(self.image.obj.height()) {
            let end = if y >= wanted.start {
                self.read_row(area)?
            } else {
                self.skip_row()?
            };
            if matches!(end, RowEnd::Missing) {
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
            RawRow::Partial(part)
                if keeps_partial_row(self.raw.total, part, self.raw.row_bytes) =>
            {
                RowEnd::Last
            }
            _ => RowEnd::Missing,
        }
    }

    /// Whether the row being read is kept if the data ends where it is read to.
    fn keeps_row(&self) -> bool {
        keeps_partial_row(self.raw.total, self.raw.at(), self.raw.row_bytes)
    }

    /// Drops the row being read, whose data ended too early to keep; `pushed` when
    /// part of it was pushed.
    fn drop_row(&self, area: &mut Area<'_>, pushed: bool) -> Result<RowEnd, Break> {
        if pushed {
            // Only the first row of the data can be too short to keep (a later row is
            // padded with at most the samples before it), so nothing else was pushed.
            debug_assert!(self.raw.total < self.raw.row_bytes);
            area.reset()?;
        }
        Ok(RowEnd::Missing)
    }

    /// Reads the next row, converting and pushing the columns `area` reads piece by
    /// piece. A row the data ends within is padded with zero samples, those past the
    /// mask's data pushed at once, or dropped.
    fn read_row(&mut self, area: &mut Area<'_>) -> Result<RowEnd, Break> {
        let columns = area.columns();
        let n = self.image.colour.components();
        let (c0, c1) = (columns.start as usize, columns.end as usize);
        let piece = self.samples.len() / n;
        self.raw.begin();
        if let Some(m) = &mut self.mask {
            m.rows.begin_row();
        }
        let mut held = match self.raw.skip(c0 * n) {
            Ok(held) => held,
            Err(e) => return Err(self.colour_failed(e)),
        };
        if !held && !self.keeps_row() {
            return Ok(RowEnd::Missing);
        }
        // Whether the mask's row holds data from `col` on.
        let mut mask_held = self.mask.is_some();
        let mut col = c0;
        while col < c1 {
            if !held && !mask_held {
                let texel = self.padding()?;
                area.push_rest(col as u32, texel)?;
                break;
            }
            let len = piece.min(c1 - col);
            let got = if held {
                match self.raw.read(&mut self.samples[..len * n]) {
                    Ok(got) => got,
                    Err(e) => return Err(self.colour_failed(e)),
                }
            } else {
                0
            };
            if got < len * n {
                if held && !self.keeps_row() {
                    return self.drop_row(area, col > c0);
                }
                held = false;
                self.samples[got..len * n].fill(0);
            }
            if let Some(m) = &mut self.mask {
                match m.rows.alpha(col..col + len, &mut m.alpha[..len]) {
                    Ok(present) => mask_held = present == len,
                    Err(e) => return Err(self.mask_failed(e)),
                }
            }
            let texels = self.texels(len)?;
            area.push_texels(col as u32, texels)?;
            col += len;
        }
        let row = if held {
            match self.raw.finish() {
                Ok(row) => row,
                Err(e) => return Err(self.colour_failed(e)),
            }
        } else {
            RawRow::Partial(self.raw.at())
        };
        if let Some(m) = &mut self.mask
            && let Err(e) = m.rows.end_row()
        {
            return Err(self.mask_failed(e));
        }
        let end = self.row_end(row);
        match end {
            RowEnd::Missing => return self.drop_row(area, true),
            _ => area.end_row()?,
        }
        Ok(end)
    }

    /// The texel to push for padded samples the mask's data does not reach either.
    fn padding(&mut self) -> Result<&[u8], Stop> {
        let n = self.image.colour.components();
        self.samples[..n].fill(0);
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
