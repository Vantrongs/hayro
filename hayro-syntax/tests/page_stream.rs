//! A page's decoded content is held once.

use hayro_syntax::Pdf;

mod common;
use common::allocations::measure;

/// Flate content is kept as decoded, not copied into the page's cache.
#[test]
fn decoded_content_is_not_copied() {
    let data = vec![b' '; (1 << 20) - 16];
    let content = common::stream("/Filter /FlateDecode", &common::stored_zlib(&data, 65_535));
    let pdf = Pdf::new(common::one_page(100, "/Contents 4 0 R", vec![content])).unwrap();
    let page = &pdf.pages()[0];
    let (content, used) = measure(|| page.page_stream().unwrap());
    assert_eq!(content, data);
    // The decoder's growing buffer fits; a second content-sized buffer does not.
    assert!(used.peak < data.len() * 3 / 2, "{used:?}");
}

/// A stream an array of contents names many times is decoded once, by the strict
/// and the lenient reading alike, though the content holds it each time.
#[test]
fn a_stream_named_many_times_is_decoded_once() {
    use hayro_syntax::work::TOLD;

    let data = common::noise(4096, 7);
    let stored = common::stored_zlib(&data, 1000);
    let content = common::stream("/Filter /FlateDecode", &stored);
    for n in [64, 128] {
        let entries = format!("/Contents [{}]", "4 0 R ".repeat(n));
        let pdf = Pdf::new(common::one_page(100, &entries, vec![content.clone()])).unwrap();
        let page = &pdf.pages()[0];
        let expected: Vec<u8> = (0..n)
            .flat_map(|_| data.iter().copied().chain(*b" "))
            .collect();
        TOLD.set(0);
        assert_eq!(page.try_page_stream().unwrap(), expected);
        let strict = TOLD.get();
        TOLD.set(0);
        assert_eq!(page.page_stream().unwrap(), expected);
        let lenient = TOLD.get();
        // One decoding takes the stored bytes and makes the data.
        let once = stored.len() + data.len();
        assert!(
            strict <= 2 * once && lenient <= 2 * once,
            "{n} names: {strict} and {lenient} bytes of work, {once} to decode once"
        );
    }
}

fn zlib(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// `zlib`, with a checksum that does not match: zlib rejects it at its end, after
/// all its data, and the permissive decoder reads it.
fn bad_checksum(data: &[u8]) -> Vec<u8> {
    let mut encoded = zlib(data);
    let last = encoded.len() - 1;
    encoded[last] ^= 1;
    encoded
}

/// Content whose streams turn out corrupt for zlib only after their data were
/// written, alone, nested, or named twice, is written again from its start with the
/// decoders that read it, and what is written after the last restart is the content.
#[test]
fn content_written_as_it_is_decoded_restarts_where_a_stream_does() {
    use hayro_syntax::object::{ObjectIdentifier, Stream};
    use hayro_syntax::page::ContentPiece;
    use hayro_syntax::work::Meter;

    let text: Vec<u8> = common::noise(300_000, 3)
        .iter()
        .map(|b| b'a' + b % 26)
        .collect();
    let objects = vec![
        common::stream("/Filter /FlateDecode", &zlib(b"0 0 m 10 10 l S")),
        common::stream("/Filter /FlateDecode", &bad_checksum(&text)),
        common::stream(
            "/Filter [/FlateDecode /FlateDecode]",
            &zlib(&bad_checksum(&text[..200_000])),
        ),
    ];
    for (contents, parts) in [
        ("5 0 R", &[5][..]),
        ("[4 0 R 5 0 R 6 0 R 4 0 R 5 0 R]", &[4, 5, 6, 4, 5][..]),
        ("[6 0 R 6 0 R]", &[6, 6][..]),
    ] {
        let entries = format!("/Contents {contents}");
        let pdf = Pdf::new(common::one_page(100, &entries, objects.clone())).unwrap();
        let decoded = |n| {
            let stream: Stream<'_> = pdf.xref().get(ObjectIdentifier::new(n, 0)).unwrap();
            stream.decoded_strict().unwrap().into_owned()
        };
        let mut expected = Vec::new();
        for &n in parts {
            expected.extend(decoded(n));
            if contents.starts_with('[') {
                expected.push(b' ');
            }
        }
        let (mut written, mut restarts) = (Vec::new(), 0);
        pdf.pages()[0]
            .try_page_stream_to(
                &mut |piece| match piece {
                    ContentPiece::Data(data) => written.extend_from_slice(data),
                    ContentPiece::Restart => {
                        restarts += 1;
                        written.clear();
                    }
                },
                &Meter::default(),
            )
            .unwrap();
        assert!(written == expected, "{contents}: {} bytes", written.len());
        // zlib rejects the data after it was written; the permissive decoder reads it.
        let expected = usize::from(cfg!(feature = "unsafe"));
        assert_eq!(restarts, expected, "{contents}");
    }
}

/// Content of many streams that each turn out corrupt for zlib after their data
/// restarts once: what is written and decoded doubles with the streams.
#[test]
#[cfg(debug_assertions)]
fn content_of_many_restarting_streams_restarts_once() {
    use hayro_syntax::page::ContentPiece;
    use hayro_syntax::work::{Meter, TOLD};

    let [small, large] = [64, 128].map(|n| {
        let objects: Vec<_> = (0..n)
            .map(|i| {
                let text: Vec<u8> = common::noise(4096, i)
                    .iter()
                    .map(|b| b'a' + b % 26)
                    .collect();
                common::stream("/Filter /FlateDecode", &bad_checksum(&text))
            })
            .collect();
        let parts: String = (0..n).map(|i| format!("{} 0 R ", 4 + i)).collect();
        let entries = format!("/Contents [{parts}]");
        let pdf = Pdf::new(common::one_page(100, &entries, objects)).unwrap();
        let (mut written, mut restarts) = (0, 0);
        TOLD.set(0);
        pdf.pages()[0]
            .try_page_stream_to(
                &mut |piece| match piece {
                    ContentPiece::Data(data) => written += data.len(),
                    ContentPiece::Restart => restarts += 1,
                },
                &Meter::default(),
            )
            .unwrap();
        assert!(restarts <= 1, "{n} streams: {restarts} restarts");
        (written, TOLD.get())
    });
    assert!(
        large.0 <= small.0 * 25 / 10 && large.1 <= small.1 * 25 / 10,
        "64 streams: {small:?}, 128: {large:?} (written, work)"
    );
}
