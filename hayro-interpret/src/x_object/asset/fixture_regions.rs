use super::*;
use hayro_syntax::Pdf;
use hayro_syntax::object::{ObjectIdentifier, Stream};
use std::time::Instant;

// TEST_IDS lists the image object numbers in TEST_PDF, as reported by pdfimages -list.
#[test]
#[ignore = "requires an external PDF and image object numbers"]
fn fixture_regions() {
    let path = std::env::var("TEST_PDF").unwrap();
    let ids = std::env::var("TEST_IDS").unwrap();
    let pdf = Pdf::new(std::fs::read(&path).unwrap()).unwrap();
    let cache = Cache::new();
    let sink: WarningSinkFn = Arc::new(|_| {});
    let mut times = [0_u128; 6];
    let mut outputs = Vec::new();
    let mut compared = 0;
    for id in ids.split_whitespace() {
        let stream = pdf
            .xref()
            .get::<Stream<'_>>(ObjectIdentifier::new(id.parse().unwrap(), 0))
            .unwrap();
        let obj = ImageXObject::new(&stream, |_| None, &sink, &cache, None).unwrap();
        let full = (obj.width(), obj.height());
        let (w, h) = full;
        let (x, y) = (w.min(32), h.min(8));
        let top = [0, 0, x, y];
        let bottom = [w - x, h - y, w, h];
        let fit = (w.min(32), h.min(32));
        let mut regions = Vec::new();
        for sequence in [
            &[(0, full, top)][..],
            &[
                (1, full, bottom),
                (2, full, top),
                (3, full, bottom),
                (3, full, top),
            ],
            &[(4, fit, [0, 0, fit.0, fit.1]), (5, full, bottom)],
        ] {
            let mut start = Instant::now();
            let asset = obj.asset(None);
            let mut source = asset.open(None).unwrap();
            assert!(source.is_streamed());
            for &(i, grid, window) in sequence {
                regions.push(
                    source
                        .request(&ImageRequest {
                            plane: Plane::Image,
                            grid,
                            window,
                            cancel: None,
                        })
                        .unwrap(),
                );
                times[i] += start.elapsed().as_micros();
                start = Instant::now();
            }
        }
        outputs.push((obj, regions));
    }
    let memory = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(str::to_owned)
        });
    let reference = std::env::var_os("TEST_REFERENCE").map(|p| std::fs::read(p).unwrap());
    let objects = outputs.len();
    for (obj, regions) in outputs {
        let whole = obj.decoded_image(None).unwrap();
        assert!(whole.alpha.is_none());
        let (data, c) = match whole.image {
            ImageData::Luma(d) => (d.data, 1),
            ImageData::Rgb(d) => (d.data, 3),
        };
        let (w, h) = (obj.width(), obj.height());
        for r in regions {
            let expected = crate::area_resample(&data, [0, 0, w, h], (w, h), c, r.grid, r.window);
            assert_eq!(expected.iter().zip(&r.data).position(|(a, b)| a != b), None);
            assert_eq!(expected.len(), r.data.len());
            if let Some(reference) = &reference {
                assert!(
                    r.data == reference[compared..compared + r.data.len()],
                    "{path} independent whole-image reference at byte {compared}"
                );
            }
            compared += r.data.len();
        }
    }
    if let Some(reference) = reference {
        assert_eq!(compared, reference.len());
    }
    println!("RESULT {path} {objects} objects {compared} bytes {times:?} us {memory:?}");
}
