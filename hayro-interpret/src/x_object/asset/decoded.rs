//! Images decoded whole when opened.

use super::rows::{PIECE_BYTES, piece_texels};
use super::{ImageLayout, PixelFormat, Plane, PlaneLayout, Stop, interleave, premultiply};
use crate::x_object::area::Area;
use crate::x_object::decode::{DecodedImage, DecodedMask, Rows};
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
    /// The rows `image`'s plane covers, and `alpha`'s, past the rows they hold.
    rows: Rows,
    alpha_rows: Rows,
}

impl Decoded {
    /// The colour and alpha of an image decoded whole.
    pub(super) fn new(decoded: DecodedImage, meter: &Meter<'_>) -> Result<Self, Cancelled> {
        let (mut image, rows, alpha) = decoded.into_parts();
        let (alpha, alpha_rows) = alpha.map_or((None, rows), |(a, r)| (Some(a), r));
        let (w, h) = (image.width(), image.height());
        // Alpha on the colour's grid holds the colour's rows (`DecodedImage`).
        let same_grid = alpha.as_ref().is_some_and(|a| {
            (a.width, a.height, alpha_rows) == (w, h, rows) && a.interpolate == image.interpolate()
        });
        if same_grid && let Some(a) = &alpha {
            let (data, c) = match &mut image {
                ImageData::Rgb(d) => (&mut d.data, 3),
                ImageData::Luma(d) => (&mut d.data, 1),
            };
            let texels = PIECE_BYTES / c;
            for (data, a) in data.chunks_mut(texels * c).zip(a.data.chunks(texels)) {
                premultiply(data, c, a);
                meter.work(data.len())?;
            }
        }
        Ok(Self {
            image,
            alpha,
            same_grid,
            stencil: false,
            rows,
            alpha_rows,
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
        let (data, c, w, held, covers) = match (plane, &self.image) {
            (Plane::Mask, _) => {
                let a = self.alpha.as_ref().ok_or(Stop::Failed)?;
                (&a.data, 1, a.width as usize, a.height, self.alpha_rows)
            }
            (Plane::Image, ImageData::Rgb(d)) => {
                (&d.data, 3, d.width as usize, d.height, self.rows)
            }
            (Plane::Image, ImageData::Luma(d)) => {
                (&d.data, 1, d.width as usize, d.height, self.rows)
            }
        };
        // Rows past those held that are transparent are not pushed.
        let rows = if covers.repeat {
            rows
        } else {
            let end = rows.end.min(held);
            rows.start.min(end)..end
        };
        let alpha = (plane == Plane::Image && self.same_grid)
            .then_some(self.alpha.as_ref())
            .flatten();
        let piece = piece_texels(c1 - c0, c + 1);
        let mut texels_with_alpha = Vec::with_capacity(piece * (c + 1));
        for y in rows.clone() {
            if y >= held {
                // The rows past those held repeat the last, all stored zero.
                let at = (held - 1) as usize * w;
                let mut texel = data[at * c..(at + 1) * c].to_vec();
                if let Some(a) = alpha {
                    texel.push(a.data[at]);
                }
                area.push_rest(c0 as u32, &texel)?;
                area.end_row()?;
                continue;
            }
            let at = y as usize * w;
            for col in (c0..c1).step_by(piece) {
                let end = (col + piece).min(c1);
                let texels = &data[(at + col) * c..(at + end) * c];
                let texels = match alpha {
                    Some(a) => {
                        interleave(
                            texels,
                            c,
                            &a.data[at + col..at + end],
                            &mut texels_with_alpha,
                        );
                        &texels_with_alpha
                    }
                    None => texels,
                };
                area.push_texels(col as u32, texels)?;
            }
            area.end_row()?;
        }
        Ok(rows.end - rows.start)
    }
}
