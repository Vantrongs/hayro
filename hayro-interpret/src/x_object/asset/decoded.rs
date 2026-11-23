//! Images decoded whole when opened.

use super::rows::{PIECE_BYTES, piece_texels};
use super::{ImageLayout, PixelFormat, Plane, PlaneLayout, Stop, interleave, premultiply};
use crate::x_object::area::Area;
use crate::x_object::decode::{DecodedImage, DecodedMask, Held, Rows};
use crate::{ImageData, LumaData};
use hayro_syntax::work::{Cancelled, Meter};

/// An image decoded whole, its planes held until it is dropped.
pub(super) struct Decoded {
    image: ImageData,
    alpha: Option<LumaData>,
    /// Whether `alpha` shares the colour's grid (the colour is then premultiplied).
    same_grid: bool,
    /// Whether `image` is a stencil mask's alpha, drawn as a mask is.
    stencil: bool,
    /// The rows `image`'s plane covers, and `alpha`'s, and their texels past those
    /// they hold: the colour's premultiplied, and with its alpha on the same grid.
    rows: Rows,
    alpha_rows: Rows,
    /// The texel past those the colour holds, as its plane is pushed.
    fill: Option<Vec<u8>>,
}

impl Decoded {
    /// The colour and alpha of an image decoded whole.
    pub(super) fn new(decoded: DecodedImage, meter: &Meter<'_>) -> Result<Self, Cancelled> {
        let (mut image, rows, alpha) = decoded.into_parts();
        let (alpha, alpha_rows) = alpha.map_or((None, rows), |(a, r)| (Some(a), r));
        let (w, h) = (image.width(), image.height());
        // Alpha on the colour's grid holds the colour's rows (`DecodedImage`).
        let same_grid = alpha.as_ref().is_some_and(|a| {
            (a.width, a.height, alpha_rows.declared) == (w, h, rows.declared)
                && a.interpolate == image.interpolate()
        });
        let c = image.channels();
        let mut fill = rows.fill.map(|f| f[..c].to_vec());
        if same_grid && let Some(a) = &alpha {
            let (data, _, _) = image.rows_mut();
            let texels = PIECE_BYTES / c;
            for (data, a) in data.chunks_mut(texels * c).zip(a.data.chunks(texels)) {
                premultiply(data, c, a);
                meter.work(data.len())?;
            }
            if let (Some(colour), Some(alpha)) = (&mut fill, alpha_rows.fill) {
                premultiply(colour, c, &alpha[..1]);
                colour.push(alpha[0]);
            }
        }
        Ok(Self {
            image,
            alpha,
            same_grid,
            stencil: false,
            rows,
            alpha_rows,
            fill,
        })
    }

    /// A stencil mask: its one plane is the alpha the paint is drawn through.
    pub(super) fn stencil(mask: DecodedMask) -> Self {
        let (luma, rows) = mask.into_parts();
        Self {
            rows,
            alpha_rows: rows,
            image: ImageData::Luma(luma),
            alpha: None,
            same_grid: false,
            stencil: true,
            fill: rows.fill.map(|f| vec![f[0]]),
        }
    }

    pub(super) fn layout(&self) -> ImageLayout {
        let (w, h) = (self.image.width(), self.rows.declared);
        let (sx, sy) = self.image.scale_factors();
        let format = match (&self.image, self.same_grid) {
            (ImageData::Luma(_), _) if self.stencil => PixelFormat::Alpha,
            (ImageData::Luma(_), false) => PixelFormat::Luma,
            (ImageData::Luma(_), true) => PixelFormat::LumaAlpha,
            (ImageData::Rgb(_), false) => PixelFormat::Rgb,
            (ImageData::Rgb(_), true) => PixelFormat::RgbAlpha,
        };
        ImageLayout {
            image: PlaneLayout {
                width: w,
                height: h,
                to_image: (sx as f64, sy as f64),
                interpolate: self.image.interpolate(),
                format,
            },
            mask: self
                .alpha
                .as_ref()
                .filter(|_| !self.same_grid)
                .map(|a| PlaneLayout {
                    width: a.width,
                    height: self.alpha_rows.declared,
                    to_image: (
                        sx as f64 * w as f64 / a.width as f64,
                        sy as f64 * h as f64 / f64::from(self.alpha_rows.declared),
                    ),
                    interpolate: a.interpolate,
                    format: PixelFormat::Alpha,
                }),
        }
    }

    /// Pushes the rows of `plane` that `area` reads and the plane covers, a piece at
    /// a time; returns how many.
    pub(super) fn run(&self, plane: Plane, area: &mut Area<'_>) -> Result<u32, Stop> {
        let (rows, cols) = (area.rows(), area.columns());
        let (c0, c1) = (cols.start as usize, cols.end as usize);
        let alpha_fill = self.alpha_rows.fill.map(|f| [f[0]]);
        let (held, c) = match (plane, &self.image) {
            (Plane::Mask, _) => {
                let a = self.alpha.as_ref().ok_or(Stop::Failed)?;
                let fill = alpha_fill.as_ref().map(|f| &f[..]);
                (Held::new(&a.data, a.width, 1, fill), 1)
            }
            (Plane::Image, ImageData::Rgb(d)) => {
                (Held::new(&d.data, d.width, 3, self.fill.as_deref()), 3)
            }
            (Plane::Image, ImageData::Luma(d)) => {
                (Held::new(&d.data, d.width, 1, self.fill.as_deref()), 1)
            }
        };
        let alpha = (plane == Plane::Image && self.same_grid)
            .then(|| {
                self.alpha
                    .as_ref()
                    .map(|a| Held::new(&a.data, a.width, 1, None))
            })
            .flatten();
        let piece = piece_texels(c1 - c0, c + 1);
        let mut texels_with_alpha = Vec::with_capacity(piece * (c + 1));
        let mut pushed = rows.start;
        for y in rows.clone() {
            if held.past(y) {
                // The texels past those held are the fill: their rows added at once,
                // in time the bins bound; rows past those held that are transparent
                // are not pushed.
                if let Some(fill) = held.fill {
                    area.push_rows(rows.end - y, fill)?;
                    pushed = rows.end;
                }
                break;
            }
            // Of a row held in part, the columns past those held are the fill.
            let row = held.row(y, c0..c1);
            let end_held = c0 + row.len() / c;
            for col in (c0..end_held).step_by(piece) {
                let end = (col + piece).min(end_held);
                let texels = &row[(col - c0) * c..(end - c0) * c];
                let texels = match &alpha {
                    Some(a) => {
                        let a = a.row(y, col..end);
                        if a.len() != end - col {
                            return Err(Stop::Failed);
                        }
                        interleave(texels, c, a, &mut texels_with_alpha);
                        &texels_with_alpha
                    }
                    None => texels,
                };
                area.push_texels(col as u32, texels)?;
            }
            if end_held < c1 {
                let fill = held.fill.ok_or(Stop::Failed)?;
                area.push_rest(end_held as u32, fill)?;
            }
            area.end_row()?;
            pushed = y + 1;
        }
        Ok(pushed - rows.start)
    }
}
