//! CCITT fax images read row by row for each request: the data decoded from its
//! start, each row's colour changes turned into the texels of the columns a request
//! reads, no row kept.

use super::rows::piece_texels;
use super::streamed::Colour;
use super::{DecodeWork, ImageLayout, PlaneLayout, Stop};
use crate::x_object::area::Area;
use crate::x_object::decode::{image_decode_params, sample_format};
use crate::x_object::image::ImageXObject;
use core::ops::Range;
use hayro_syntax::object::dict::keys::{MASK, SMASK, SMASK_IN_DATA};
use hayro_syntax::object::stream::{CcittEnd, CcittRows, ImageColorSpace, ImageData};
use hayro_syntax::work::Meter;
use std::sync::OnceLock;

/// The rows a CCITT image's data decodes to and the rows its plane has, counted when
/// its asset is first opened; `None` when decoding it whole fails.
pub(super) type CcittHeight = OnceLock<Option<Counted>>;

#[derive(Clone, Copy)]
pub(super) struct Counted {
    /// The rows the data decodes to.
    decoded: u32,
    /// The rows of the plane: those decoded, and white ones after them where whole
    /// decoding pads the image.
    kept: u32,
}

/// A CCITT image read row by row.
pub(super) struct CcittStreamed<'a> {
    rows: CcittRows<'a>,
    colour: Colour,
    counted: Counted,
    layout: PlaneLayout,
}

impl<'a> CcittStreamed<'a> {
    /// `obj` read row by row, if its one filter is `CCITTFaxDecode`, it has no mask,
    /// and it is a gray image or a stencil mask whose 8-bit rows [`Colour`] converts;
    /// its rows are counted into `height` once, by decoding all of its data. Fails
    /// where decoding it whole fails.
    pub(super) fn new(
        obj: &ImageXObject<'a>,
        height: &CcittHeight,
        meter: &Meter<'_>,
    ) -> Result<Option<Self>, Stop> {
        let dict = obj.stream.dict();
        if [SMASK, MASK, SMASK_IN_DATA]
            .iter()
            .any(|k| dict.contains_key(k))
        {
            return Ok(None);
        }
        let params = image_decode_params(obj);
        let rows = obj.stream.ccitt_rows(&params, meter);
        meter.poll()?;
        let Some(rows) = rows else {
            return Ok(None);
        };
        // What the filter reports of the samples it decodes, as whole decoding reads
        // them.
        let data = ImageData {
            alpha: None,
            color_space: Some(ImageColorSpace::Gray),
            bits_per_component: 8,
            width: rows.columns(),
            height: params.height,
        };
        let Some(colour) =
            Colour::of(obj, sample_format(obj, Some(&data))).filter(|c| c.components() == 1)
        else {
            return Ok(None);
        };
        let counted = match height.get() {
            Some(&counted) => counted,
            None => {
                let counted = count(&rows, meter)?;
                *height.get_or_init(|| counted)
            }
        };
        let counted = counted.ok_or(Stop::Failed)?;
        let (w, h) = (obj.width(), obj.height());
        let layout = PlaneLayout {
            width: rows.columns(),
            height: counted.kept,
            // As whole decoding maps a plane of another size than the dictionary
            // declares onto the image.
            to_image: (
                f64::from(w as f32 / rows.columns() as f32),
                f64::from(h as f32 / params.height as f32),
            ),
            interpolate: obj.interpolate,
            format: colour.format(false),
        };
        Ok(Some(Self {
            rows,
            colour,
            counted,
            layout,
        }))
    }

    pub(super) fn layout(&self) -> ImageLayout {
        ImageLayout {
            image: self.layout,
            mask: None,
        }
    }

    /// Reads the image plane into `area`: decodes the data from its start through
    /// the last row `area` reads, converting and pushing the rows it reads.
    pub(super) fn run(&self, area: &mut Area<'_>, meter: &Meter<'_>) -> Result<DecodeWork, Stop> {
        let wanted = area.rows();
        let columns = area.columns();
        let columns = columns.start as usize..columns.end as usize;
        let mut texels = vec![0; piece_texels(columns.len(), 1)];
        let mut rgb = Vec::new();
        let mut y = 0;
        let mut failed = None;
        let held = wanted.end.min(self.counted.decoded);
        if held > 0 {
            let end = self.rows.decode(
                |changes| {
                    if y >= wanted.start
                        && let Err(stop) =
                            self.push_row(area, changes, columns.clone(), &mut texels, &mut rgb)
                    {
                        failed = Some(stop);
                        return false;
                    }
                    y += 1;
                    y < held
                },
                meter,
            );
            if let Some(stop) = failed {
                return Err(stop);
            }
            if end == CcittEnd::Cancelled {
                return Err(Stop::Cancelled);
            }
            // The data decodes as it did when its rows were counted.
            if y < held {
                return Err(Stop::Failed);
            }
        }
        // Rows past those the data holds are white, as whole decoding pads them.
        let mut white = [self.rows.white()];
        let white = self.colour.convert(&mut white, &mut rgb)?;
        for _ in y.max(wanted.start)..wanted.end {
            area.push_rest(columns.start as u32, white)?;
            area.end_row()?;
        }
        Ok(DecodeWork {
            rows_decoded: u64::from(y),
            bytes_inflated: 0,
        })
    }

    /// Pushes `columns` of a row whose colour changes at `changes`, a piece of
    /// `texels` at a time.
    fn push_row(
        &self,
        area: &mut Area<'_>,
        changes: &[u32],
        columns: Range<usize>,
        texels: &mut [u8],
        rgb: &mut Vec<u8>,
    ) -> Result<(), Stop> {
        let white = self.rows.white();
        let black = !white;
        // The change after the first column: an odd number of changes at or before a
        // pixel makes it black.
        let mut next = changes.partition_point(|&c| c as usize <= columns.start);
        let mut x = columns.start;
        while x < columns.end {
            let end = (x + texels.len()).min(columns.end);
            let piece = &mut texels[..end - x];
            let mut at = x;
            while at < end {
                let colour = if next % 2 == 1 { black } else { white };
                let change = changes.get(next).map_or(usize::MAX, |&c| c as usize);
                let to = change.min(end);
                piece[at - x..to - x].fill(colour);
                at = to;
                if at == change {
                    next += 1;
                }
            }
            let converted = self.colour.convert(piece, rgb)?;
            area.push_texels(x as u32, converted)?;
            x = end;
        }
        area.end_row()?;
        Ok(())
    }
}

/// The rows `rows` decodes to and keeps, decoding all of its data; `None` where
/// decoding it whole fails.
fn count(rows: &CcittRows<'_>, meter: &Meter<'_>) -> Result<Option<Counted>, Stop> {
    let mut decoded = 0;
    let end = rows.decode(
        |_| {
            decoded += 1;
            true
        },
        meter,
    );
    if end == CcittEnd::Cancelled {
        return Err(Stop::Cancelled);
    }
    Ok(rows
        .rows_kept(decoded, end)
        .map(|kept| Counted { decoded, kept }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        assert_asset_streams_as_decoded, asset, image, is_streamed, named, pdf, request, transfer,
        whole,
    };
    use super::super::{Plane, RequestError};
    use crate::x_object::decode::decode_image;
    use std::sync::Arc;

    /// A 211 × 96 test page in each encoding, as `hayro-ccitt`'s tests have it.
    const G4: &[u8] = include_bytes!("../../../../hayro-ccitt/tests/streams/g4.bin");
    const G3_1D: &[u8] = include_bytes!("../../../../hayro-ccitt/tests/streams/g3_1d.bin");
    const G3_2D: &[u8] = include_bytes!("../../../../hayro-ccitt/tests/streams/g3_2d.bin");
    /// The G4 page ended by an end-of-block code.
    const G4_EOFB: &[u8] = include_bytes!("../../../../hayro-ccitt/tests/streams/g4_eofb.bin");

    fn ccitt(k: i32, extra: &str) -> String {
        format!("/Filter /CCITTFaxDecode /DecodeParms << /K {k} /Columns 211 {extra} >>")
    }

    /// Images and stencil masks of CCITT data stream as they decode whole, whatever
    /// the encoding, `BlackIs1` and `/Decode`, under a transfer function, and with a
    /// `/Width` other than the data's columns.
    #[test]
    fn ccitt_images_stream_as_decoded() {
        for (k, data) in [(-1, G4), (0, G3_1D), (1, G3_2D)] {
            for kind in [
                "/ColorSpace /DeviceGray /BitsPerComponent 1",
                "/ColorSpace /DeviceGray /BitsPerComponent 1 /Decode [1 0]",
                "/ImageMask true",
                "/ImageMask true /Decode [1 0]",
            ] {
                for params in ["", "/BlackIs1 true"] {
                    for width in [211, 100] {
                        let dict = format!("{kind} {}", ccitt(k, params));
                        let pdf = pdf(&[(image(&dict, width, 96), data.to_vec())]);
                        let name = format!("K {k}, {kind}, {params}, width {width}");
                        let check = |four: Option<bool>| {
                            let mut asset = asset(&pdf);
                            if let Some(four) = four {
                                Arc::get_mut(&mut asset.0).unwrap().transfer_function =
                                    Some(transfer(four));
                            }
                            named(&format!("{name}, transfer {four:?}"), || {
                                assert_asset_streams_as_decoded(&asset);
                            });
                        };
                        check(None);
                        if !kind.contains("ImageMask") {
                            check(Some(false));
                            check(Some(true));
                        }
                    }
                }
            }
        }
    }

    /// Data that ends before the image's rows: the missing rows are white where whole
    /// decoding pads them (few, after data that decodes through), and left out
    /// otherwise; data that breaks off keeps the rows before; data that holds no row
    /// fails as decoding it whole does.
    #[test]
    fn ccitt_data_that_ends_early_streams_as_decoded() {
        let gray = "/ColorSpace /DeviceGray /BitsPerComponent 1";
        for (height, data, rows) in [
            // Padded: fewer missing rows than rows decoded.
            (150, G4_EOFB.to_vec(), 150..151),
            // Left out: more missing rows than rows decoded, and above the small
            // image's padding.
            (5_000, G4_EOFB.to_vec(), 96..97),
            // Left out: the data, without an end-of-block code, fails past its rows.
            (150, G4.to_vec(), 96..97),
            // Broken off within its rows.
            (96, G4[..G4.len() / 2].to_vec(), 1..96),
        ] {
            let pdf = pdf(&[(
                image(&format!("{gray} {}", ccitt(-1, "")), 211, height),
                data,
            )]);
            let asset = asset(&pdf);
            named(&format!("height {height}"), || {
                assert_asset_streams_as_decoded(&asset);
            });
            let source = asset.open(None, None).unwrap();
            let kept = source.layout().image.height;
            assert!(rows.contains(&kept), "height {height}: {kept} rows");
        }
        // Data that holds no row: where whole decoding fails, so does opening the
        // image.
        let mut failed = 0;
        for data in [
            vec![0xFF; 4],
            vec![0; 4],
            vec![0x12, 0x34, 0x56, 0x78],
            vec![],
        ] {
            let pdf = pdf(&[(image(&format!("{gray} {}", ccitt(-1, "")), 211, 96), data)]);
            let asset = asset(&pdf);
            let meter = hayro_syntax::work::Meter::default();
            let decodes = decode_image(&asset.xobject().unwrap(), None, &meter).is_some();
            match asset.open(None, None) {
                Err(RequestError::Failed) => {
                    assert!(!decodes);
                    failed += 1;
                }
                Ok(_) => {
                    assert!(decodes);
                    assert_asset_streams_as_decoded(&asset);
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        assert!(failed > 0);
    }

    /// A request decodes the rows through the last one it reads and no further; the
    /// rows are counted once, when the asset is first opened.
    #[test]
    fn a_ccitt_request_decodes_through_its_rows() {
        let gray = "/ColorSpace /DeviceGray /BitsPerComponent 1";
        let pdf = pdf(&[(
            image(&format!("{gray} {}", ccitt(-1, "")), 211, 96),
            G4.to_vec(),
        )]);
        let asset = asset(&pdf);
        let mut source = asset.open(None, None).unwrap();
        assert!(is_streamed(&source));
        let a = request(&mut source, Plane::Image, (211, 96), [10, 20, 30, 40]);
        assert_eq!(source.work().rows_decoded, 40);
        let b = request(
            &mut whole(&asset),
            Plane::Image,
            (211, 96),
            [10, 20, 30, 40],
        );
        assert_eq!(a.data, b.data);
        assert!(super::super::ccitt::CcittHeight::get(&asset.0.ccitt_height).is_some());
    }
}
