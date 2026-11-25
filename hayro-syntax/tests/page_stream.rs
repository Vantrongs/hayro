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

#[test]
fn cache_reuses_content_unless_a_referenced_object_was_lost() {
    for (entries, lost) in [
        ("[4 0 R]", false),
        ("[4 0 R null]", false),
        ("[4 0 R 5 0 R]", false),
        ("[4 0 R 6 0 R]", true),
    ] {
        let pdf = Pdf::new(common::one_page(
            100,
            &format!("/Contents {entries}"),
            vec![
                common::stream("", b"q Q"),
                b"null".to_vec(),
                b"<< /Length 9999 >>\nstream\nq".to_vec(),
            ],
        ))
        .unwrap();
        let page = &pdf.pages()[0];
        let mut cache = hayro_syntax::page::PageStreamCache::default();
        let mut read = || {
            let (bytes, missing) = pdf.xref().watch(|| cache.get(page).unwrap());
            assert_eq!(missing.is_some(), lost, "{entries}");
            assert_eq!(bytes.as_slice(), b"q Q ");
            bytes
        };
        assert_eq!(std::sync::Arc::ptr_eq(&read(), &read()), !lost, "{entries}");
    }
}

/// A content under a predictor whose last row is cut short keeps that row, by the
/// strict and the lenient reading alike, as `MuPDF` draws it.
#[test]
fn a_content_row_cut_short_is_kept() {
    let content = common::cut_row_content_stream();
    let pdf = Pdf::new(common::one_page(100, "/Contents 4 0 R", vec![content])).unwrap();
    let page = &pdf.pages()[0];
    assert_eq!(page.try_page_stream().unwrap(), common::CUT_ROW_CONTENT);
    assert_eq!(page.page_stream().unwrap(), common::CUT_ROW_CONTENT);
}

/// A stream an array of contents names many times is decoded once, by the strict
/// and the lenient reading alike, though the content holds it each time.
#[test]
fn a_stream_named_many_times_is_decoded_once() {
    use hayro_syntax::work::{STATE_MADE, TOLD};

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
        // The work of the data: a decoder's state made counts apart.
        let work = || TOLD.get() - STATE_MADE.get();
        TOLD.set(0);
        STATE_MADE.set(0);
        assert_eq!(page.try_page_stream().unwrap(), expected);
        let strict = work();
        TOLD.set(0);
        STATE_MADE.set(0);
        assert_eq!(page.page_stream().unwrap(), expected);
        let lenient = work();
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

/// A part of an array of contents that is no stream is passed over, not the end of
/// the content, as `MuPDF` reads it (`PyMuPDF` 1.28.2 draws the parts before and
/// after a null, a reference to no object, a number and a dictionary). The strict
/// reading passes over a null and a reference to no object too, and refuses a part
/// of another kind.
#[test]
fn a_part_of_the_contents_that_is_no_stream_is_passed_over() {
    let objects = || {
        vec![
            common::stream("", b"1 0 0 rg"),
            common::stream("", b"0 0 10 10 re f"),
            b"<< /A 1 >>".to_vec(),
        ]
    };
    for (contents, strict) in [
        ("[4 0 R null 9 0 R 5 0 R]", true),
        ("[4 0 R 6 0 R 5 0 R]", false),
        ("[4 0 R 7 5 0 R]", false),
    ] {
        let entries = format!("/Contents {contents}");
        let pdf = Pdf::new(common::one_page(100, &entries, objects())).unwrap();
        let page = &pdf.pages()[0];
        assert_eq!(
            page.page_stream().unwrap(),
            b"1 0 0 rg 0 0 10 10 re f ",
            "{contents}"
        );
        assert_eq!(page.try_page_stream().is_ok(), strict, "{contents}");
        if strict {
            assert_eq!(page.try_page_stream().unwrap(), b"1 0 0 rg 0 0 10 10 re f ");
        }
    }
}

#[test]
fn distinct_streams_share_only_the_collected_output() {
    let objects = (0..32)
        .map(|i| common::stream("/Filter /FlateDecode", &zlib(&vec![i; 65535])))
        .collect();
    let refs = (4..36).map(|id| format!("{id} 0 R ")).collect::<String>();
    let pdf = Pdf::new(common::one_page(
        100,
        &format!("/Contents [{refs}]"),
        objects,
    ))
    .unwrap();
    let (data, used) = measure(|| pdf.pages()[0].read_page_stream().unwrap());
    assert_eq!(data.len(), 32 * 65536);
    for (i, part) in data.as_chunks::<65536>().0.iter().enumerate() {
        assert!(part[..65535].iter().all(|&v| v == i as u8));
        assert_eq!(part[65535], b' ');
    }
    assert!(used.peak < data.len() + data.len() / 3, "{used:?}");
}

#[test]
fn recovered_filter_prefix_is_reused() {
    for (contents, expected) in [("4 0 R", b"q Q".as_slice()), ("[4 0 R]", b"q Q ")] {
        let pdf = Pdf::new(common::one_page(
            10,
            &format!("/Contents {contents}"),
            vec![common::stream("/Filter /ASCIIHexDecode", b"712051x>")],
        ))
        .unwrap();
        let mut cache = hayro_syntax::page::PageStreamCache::default();
        let first = cache.get(&pdf.pages()[0]).unwrap();
        let second = cache.get(&pdf.pages()[0]).unwrap();
        assert_eq!(first.as_slice(), expected);
        assert_eq!(first, second);
        assert!(std::sync::Arc::ptr_eq(&first, &second));
    }
}

#[test]
fn short_escaped_operators_allocate_nothing() {
    let data = b"#71 #51 ".repeat(100);
    let (_, allocations) = measure(|| {
        let mut iter = hayro_syntax::content::UntypedIter::new(&data);
        for _ in 0..100 {
            assert_eq!(iter.next().unwrap().operator.as_ref(), b"q");
            assert_eq!(iter.next().unwrap().operator.as_ref(), b"Q");
        }
        assert!(iter.next().is_none());
    });
    assert_eq!(allocations.count, 0);
}

#[test]
fn escaped_operator_storage_matches_decoded_length() {
    for len in [15, 16, 22, 23] {
        let data = b"#71".repeat(len);
        let (op, allocated) = measure(|| {
            hayro_syntax::content::UntypedIter::new(&data)
                .next()
                .unwrap()
                .operator
                .clone()
        });
        assert_eq!(op.as_ref(), vec![b'q'; len]);
        assert_eq!(allocated.count == 0, len <= 22);
        if len > 22 {
            let (copy, allocated) = measure(|| op.clone());
            assert_eq!(allocated.count, 0);
            assert_eq!(copy.as_ptr(), op.as_ptr());
        }
    }
}

#[test]
fn unescaped_operator_borrows_the_input() {
    let data = b"longUnescapedOperatorName";
    let (op, allocated) = measure(|| {
        hayro_syntax::content::UntypedIter::new(data)
            .next()
            .unwrap()
            .operator
            .clone()
    });
    assert_eq!(allocated.count, 0);
    assert_eq!(op.as_ref(), data);
    assert_eq!(op.as_ptr(), data.as_ptr());
}
