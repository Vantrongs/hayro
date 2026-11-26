//! Invariants of transactional decoding and its filter stages.
#[path = "common/allocations.rs"]
mod allocations;
#[path = "forward/chains.rs"]
mod chains;
#[path = "common/cpu_time.rs"]
mod cpu_time;
#[path = "forward/fixtures.rs"]
mod fixtures;
use chains::{Rng, ascii_hex, damaged, data, layer, zlib};
use fixtures::{file, noise, stored_zlib, stream};
use hayro_syntax::{
    Pdf,
    object::stream::DecodeFailure,
    object::{ObjectIdentifier, Stream},
};

fn stream_file(dict: &str, data: &[u8]) -> Pdf {
    Pdf::new(file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            stream(dict, data),
        ],
        "",
    ))
    .unwrap()
}

fn object_3(pdf: &Pdf) -> Stream<'_> {
    pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap()
}

#[test]
fn predictor_parameters_use_the_written_integer_at_the_filter_position() {
    let pdf = stream_file(
        "/Filter [null /Crypt /FlateDecode] /DP [null null << /Predictor 12 /Colors 4294967297 /Columns 3 >>]",
        &stored_zlib(&[1, 10, 2, 3], 8),
    );
    let stream = object_3(&pdf);
    assert_eq!(stream.decoded_strict().unwrap().as_ref(), &[10, 12, 15]);
    assert_eq!(stream.decoded_data().unwrap().as_ref(), &[10, 12, 15]);
}

#[test]
fn the_reader_reads_what_strict_decoding_returns() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for case in 0..3000 {
        let mut encoded = data(&mut rng);
        let mut stages = Vec::new();
        for _ in 0..1 + rng.below(3) {
            let (name, params, bytes) = layer(&encoded, &mut rng);
            encoded = damaged(bytes, &mut rng);
            stages.insert(0, (name, params));
        }
        let dict = format!(
            "/Filter [{}] /DecodeParms [{}]",
            stages
                .iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            stages
                .iter()
                .map(|s| s.1.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let pdf = stream_file(&dict, &encoded);
        let object = object_3(&pdf);
        let actual = object.decoded_strict().ok();
        let mut streamed = Vec::new();
        let result = object.drain_strict(&mut |bytes| streamed.extend_from_slice(bytes));
        assert!(
            result.ok().map(|()| streamed.as_slice()) == actual.as_deref(),
            "streamed case {case}, {dict}"
        );
        let expected = stages.iter().try_fold(encoded, |input, (name, params)| {
            let pdf = stream_file(&format!("/Filter {name} /DecodeParms {params}"), &input);
            object_3(&pdf).decoded_strict().ok().map(|v| v.into_owned())
        });
        assert!(
            actual.as_deref() == expected.as_deref(),
            "case {case} {dict} {:?} vs {:?}",
            actual.as_ref().map(|v| v.len()),
            expected.as_ref().map(Vec::len)
        );
    }
}

#[test]
fn an_end_does_not_hide_a_failure_before_it() {
    let mut hex = ascii_hex(&zlib(b"0 0 m 10 10 l S", 6), &mut Rng(3));
    hex.retain(|&b| b != b'>');
    hex.extend(b"00".repeat(1 << 20));
    hex.extend(b"Q>");
    let mut text = b"414243>".to_vec();
    text.extend(noise(100_000, 9));
    let mut flate = vec![0x78, 1];
    for block in text.chunks(65535) {
        let len = block.len() as u16;
        flate.push(0);
        flate.extend(len.to_le_bytes());
        flate.extend((!len).to_le_bytes());
        flate.extend(block);
    }
    flate.push(7);
    for (dict, input) in [("/Filter [/AHx /Fl]", hex), ("/Filter [/Fl /AHx]", flate)] {
        assert!(matches!(
            object_3(&stream_file(dict, &input)).decoded_strict(),
            Err(DecodeFailure::StreamDecode)
        ));
    }
}

#[test]
fn deep_expanding_chains_read_what_strict_decoding_returns() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut cases: Vec<(usize, Vec<usize>)> = vec![
        (12, vec![]),
        (12, vec![5]),
        (12, vec![11]),
        (12, vec![0]),
        (12, (0..12).collect()),
        (40, vec![]),
        (40, vec![3, 20]),
    ];
    for _ in 0..6 {
        let n = 2 + rng.below(11);
        cases.push((n, (0..n).filter(|_| rng.chance(25)).collect()));
    }
    for (case, (n, damaged)) in cases.into_iter().enumerate() {
        let content = noise(if n > 17 { 1_200_000 } else { 3_000_000 }, case as u32);
        let mut encoded = content.clone();
        for i in 0..n {
            encoded = if rng.below(3) == 0 {
                zlib(&encoded, 1)
            } else {
                stored_zlib(&encoded, 65535)
            };
            if damaged.contains(&i) {
                *encoded.last_mut().unwrap() ^= 1;
            }
        }
        let pdf = stream_file(&format!("/Filter [{}]", vec!["/Fl"; n].join(" ")), &encoded);
        assert!(object_3(&pdf).decoded_strict().unwrap()[..] == content);
    }
}

#[cfg(feature = "images")]
#[test]
fn flate_before_fax_emits_only_completed_packed_rows() {
    let data = zlib(&[0x35, 0x14], 6);
    let pdf = stream_file(
        "/Filter [/Fl /CCF] /DecodeParms [null << /K 0 /Columns 8 /Rows 2147483647 >>]",
        &data,
    );
    let result = object_3(&pdf)
        .decoded_image(&hayro_syntax::object::stream::ImageDecodeParams::default())
        .unwrap();
    assert_eq!(&*result.data, &[0]);
    let mut cursor = object_3(&pdf).decoded_reader(None).unwrap();
    let mut row = [0xff; 1];
    assert_eq!(cursor.read(&mut row).unwrap(), 1);
    assert_eq!(row, [0]);
    assert_eq!(cursor.read(&mut row).unwrap(), 0);
    assert_eq!(result.image_data.unwrap().bits_per_component, 1);
}

#[test]
fn plain_stream_emits_its_borrowed_bytes() {
    let doc = stream_file("", b"q Q");
    let stream = object_3(&doc);
    let data = stream.try_raw_data().unwrap();
    let mut calls = 0;
    stream
        .drain_strict(&mut |chunk| {
            assert!(core::ptr::eq(chunk, data.as_ref()));
            calls += 1;
        })
        .unwrap();
    assert_eq!(calls, 1);
}

#[test]
fn streaming_probes_ambiguous_flate_before_emitting() {
    let expected = noise(100_000, 31);
    let mut raw = fixtures::stored_deflate(&expected, 0x9c);
    raw[0] = 0x78;
    for (filters, data) in [("/Fl", raw.clone()), ("[/Fl /Fl]", zlib(&raw, 6))] {
        let pdf = stream_file(&format!("/Filter {filters}"), &data);
        let mut output = Vec::new();
        object_3(&pdf)
            .drain_strict(&mut |bytes| output.extend_from_slice(bytes))
            .unwrap();
        assert_eq!(output, expected);
    }
}

#[test]
fn large_page_content_drains_with_bounded_memory() {
    let content = b"q Q ".repeat(2 * 1024 * 1024);
    let encoded = zlib(&content, 6);
    let doc = Pdf::new(file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec(),
            stream("/Filter /Fl", &encoded),
        ],
        "",
    ))
    .unwrap();
    let page = &doc.pages()[0];
    let mut pos = 0;
    let ((), used) = allocations::measure(|| {
        page.try_page_stream_to(&mut |chunk| {
            assert_eq!(chunk, &content[pos..pos + chunk.len()]);
            pos += chunk.len();
        })
        .unwrap();
    });
    assert_eq!(pos, content.len());
    eprintln!(
        "{} compressed bytes, {pos} decoded bytes, {} peak heap bytes",
        encoded.len(),
        used.peak
    );
    assert!(used.peak < 512 * 1024);
}

#[test]
fn malformed_huffman_tables_are_rejected() {
    for input in [
        &[213, 49, 13, 192, 48, 20, 109][..],
        &[244, 248, 3],
        &[120, 94, 237, 208, 169, 107, 237, 240, 189, 174, 46],
    ] {
        let doc = Pdf::new(file(
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec(),
                stream("/Filter /Fl", input),
            ],
            "",
        ))
        .unwrap();
        let mut output = Vec::new();
        assert!(
            doc.pages()[0]
                .try_page_stream_to(&mut |chunk| output.extend_from_slice(chunk))
                .is_err()
        );
        assert!(output.is_empty());
    }
}

#[test]
fn strict_filters_require_complete_packets() {
    for (filter, data, expected) in [
        ("/Fl", vec![], None),
        ("/A85", b"z~".to_vec(), None),
        (
            "/A85",
            [vec![32; 8190], b"z~>".to_vec()].concat(),
            Some(&[0; 4][..]),
        ),
        ("/Fl", zlib(b"", 6), Some(&b""[..])),
        ("/RL", vec![2, b'a'], None),
        ("/RL", vec![2, b'a', b'b', b'c', 128], Some(&b"abc"[..])),
    ] {
        let pdf = stream_file(&format!("/Filter {filter}"), &data);
        assert_eq!(object_3(&pdf).decoded_strict().ok().as_deref(), expected);
    }
}

#[test]
fn flate_recovery_work_grows_with_the_number_of_stages() {
    let input = noise(16_384, 73);
    let cost = |count| {
        let mut encoded = input.clone();
        for _ in 0..count {
            encoded = stored_zlib(&encoded, 65535);
            *encoded.last_mut().unwrap() ^= 1;
        }
        let pdf = stream_file(
            &format!("/Filter [{}]", vec!["/Fl"; count].join(" ")),
            &encoded,
        );
        let stream = object_3(&pdf);
        let mut times = [std::time::Duration::ZERO; 5];
        let mut allocated = 0;
        for time in &mut times {
            let start = cpu_time::thread_cpu_time();
            let (decoded, work) = allocations::measure(|| stream.decoded_strict().unwrap());
            *time = cpu_time::thread_cpu_time() - start;
            assert_eq!(decoded.as_ref(), input);
            allocated = work.allocated;
        }
        times.sort();
        (allocated, times[2])
    };
    // Each pass through a stage allocates an inflater and its output.
    let small = cost(4);
    let large = cost(16);
    assert!(
        large.0 < 6 * small.0,
        "stage allocations: {small:?}, {large:?}"
    );
    assert!(
        large.1 < 8 * small.1,
        "stage decoding time: {small:?}, {large:?}"
    );
}

#[test]
fn flate_recovery_streams_expanded_whitespace_to_the_next_filter() {
    let mut bytes = vec![b' '; 2 << 20];
    bytes.extend(b"41>");
    let mut encoded = zlib(&bytes, 6);
    *encoded.last_mut().unwrap() ^= 1;
    let pdf = stream_file("/Filter [/Fl /AHx]", &encoded);
    let stream = object_3(&pdf);
    let (result, used) = allocations::measure(|| stream.decoded_strict().unwrap());
    assert_eq!(result.as_ref(), b"A");
    assert!(used.peak < bytes.len() / 2, "{used:?}");
}

#[test]
fn raw_stored_block_with_a_zlib_header_recovers_after_a_prefix_filter() {
    let encoded = b"780100FEFF41010000FFFF>";
    let pdf = stream_file("/Filter [/AHx /Fl]", encoded);
    assert_eq!(object_3(&pdf).decoded_strict().unwrap().as_ref(), b"A");
}

#[test]
fn short_flate_image_allocates_for_the_decoded_row() {
    let pdf = stream_file("/Filter /FlateDecode", &stored_zlib(&[19; 512], 512));
    let stream = object_3(&pdf);
    let params = hayro_syntax::object::stream::ImageDecodeParams {
        width: 512,
        height: 1_000_000,
        num_components: Some(1),
        bpc: Some(8),
        ..Default::default()
    };
    let (result, used) = allocations::measure(|| stream.decoded_image(&params).unwrap());
    assert_eq!(&*result.data, &[19; 512]);
    assert!(used.peak < 1024 * 1024, "{used:?}");
}

#[cfg(feature = "images")]
#[test]
fn short_fax_allocates_for_the_decoded_row_in_both_formats() {
    let pdf = stream_file(
        "/Filter /CCITTFaxDecode /DecodeParms << /Columns 8 >>",
        &[0x35, 0x14],
    );
    let stream = object_3(&pdf);
    let params = hayro_syntax::object::stream::ImageDecodeParams {
        width: 8,
        height: 1_000_000,
        ..Default::default()
    };
    for luma in [false, true] {
        let (result, used) = allocations::measure(|| {
            if luma {
                stream.decoded_luma_image(&params)
            } else {
                stream.decoded_image(&params)
            }
            .unwrap()
        });
        let info = result.image_data.unwrap();
        assert_eq!(info.bits_per_component, if luma { 8 } else { 1 });
        assert_eq!(info.height, params.height);
        assert_eq!(result.data.len(), if luma { 8 } else { 1 });
        assert!(used.peak < 1024 * 1024, "{used:?}");
    }
}

#[cfg(feature = "images")]
#[test]
fn short_baseline_jpeg_stops_before_a_whole_frame_allocation() {
    let pdf =
        Pdf::new(include_bytes!("../../tests/pdfs/custom/image_luma8_jpg.pdf").to_vec()).unwrap();
    let stream: Stream<'_> = pdf.xref().get(ObjectIdentifier::new(4, 0)).unwrap();
    let hex = stream_file("/Filter /ASCIIHexDecode", &stream.raw_data());
    let hex_stream = object_3(&hex);
    let data = hex_stream.decoded_data().unwrap();
    let pdf = stream_file(
        "/Filter /DCTDecode",
        &data[..data
            .windows(2)
            .position(|marker| marker == [0xff, 0xda])
            .unwrap()
            + 10],
    );
    let params = hayro_syntax::object::stream::ImageDecodeParams {
        width: 200,
        height: 200,
        num_components: Some(1),
        bpc: Some(8),
        ..Default::default()
    };
    let stream = object_3(&pdf);
    let (result, used) = allocations::measure(|| stream.decoded_image(&params));
    assert!(matches!(result, Err(DecodeFailure::ImageDecode)));
    assert!(used.largest < 200 * 200, "{used:?}");
}

#[cfg(all(any(debug_assertions, feature = "work-counters"), feature = "std"))]
#[test]
fn captured_stream_replays_after_document_drop_without_reading_an_indirect_body() {
    use hayro_syntax::work::BODIES_READ;
    let pdf = Pdf::new(file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            stream("/Filter 4 0 R /DecodeParms 5 0 R", b"4142>"),
            b"/ASCIIHexDecode".to_vec(),
            b"<< >>".to_vec(),
        ],
        "",
    ))
    .unwrap();
    let captured = object_3(&pdf).to_owned_stream();
    drop(pdf);
    assert_eq!(captured.copied_bytes(), 0);
    BODIES_READ.set(0);
    for _ in 0..2 {
        assert_eq!(
            captured.get().unwrap().decoded_data().unwrap().as_ref(),
            b"AB"
        );
    }
    assert_eq!(BODIES_READ.get(), 0);
}
