//! A predictor is reversed as `MuPDF` reverses it (observed in `PyMuPDF` 1.28.2): a last
//! row cut short is kept, every row filter and pixel size is unfiltered a byte at a
//! time, its parameters are read as `MuPDF` reads them, and parameters it refuses fail
//! decoding; by whole decoding, strict and lenient, and by the readers that decode a
//! piece at a time. Data cut short in the other filters keep what they hold, where
//! `MuPDF` reads less or adds a byte.

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::ReadError;
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::Meter;
use std::io::Write;

mod common;

/// A document whose object 3 is a stream with `dict` entries holding `data`.
fn stream_file(dict: &str, data: &[u8]) -> Pdf {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
        common::stream(dict, data),
    ];
    Pdf::new(common::file(&objects, "")).unwrap()
}

fn object_3(pdf: &Pdf) -> Stream<'_> {
    pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// `data` LZW-encoded in 9-bit codes, which suffice for short data: a clear-table
/// code, each byte, the end-of-data code.
fn lzw(data: &[u8]) -> Vec<u8> {
    let codes = [256].into_iter().chain(data.iter().map(|&b| u32::from(b)));
    let (mut bits, mut count, mut out) = (0_u64, 0, Vec::new());
    for code in codes.chain([257]) {
        bits = bits << 9 | u64::from(code);
        count += 9;
        while count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    out.push((bits << (8 - count)) as u8);
    out
}

/// All that `read` gives, three bytes at a time; `None` where it fails.
fn read_all(mut read: impl FnMut(&mut [u8]) -> Result<usize, ReadError>) -> Option<Vec<u8>> {
    let mut buf = [0; 3];
    let mut out = Vec::new();
    loop {
        match read(&mut buf) {
            Ok(0) => return Some(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(ReadError::Restarted) => out.clear(),
            Err(_) => return None,
        }
    }
}

/// What every way of decoding `stream` gives: whole, strict and lenient, and read a
/// piece at a time by each reader that takes the stream; `None` for a failure.
fn every_reading(stream: &Stream<'_>) -> Vec<(&'static str, Option<Vec<u8>>)> {
    let mut readings = vec![
        ("decoded", stream.decoded().ok().map(|d| d.into_owned())),
        (
            "decoded_strict",
            stream.decoded_strict().ok().map(|d| d.into_owned()),
        ),
    ];
    if let Some(mut reader) = stream.decoded_strict_reader(&Meter::default()).unwrap() {
        let read = read_all(|buf| reader.read(buf, &Meter::default()));
        readings.push(("decoded_strict_reader", read));
    }
    if let Some(mut reader) = stream.decoded_reader() {
        let read = read_all(|buf| Ok(reader.read(buf, &Meter::default())?));
        readings.push(("decoded_reader", read));
    }
    readings
}

/// The decode parameters, the rows as stored before compression, and what `PyMuPDF`
/// 1.28.2 decodes them to (`Document::xref_stream`).
const CASES: &[(&str, &[u8], &[u8])] = &[
    // PNG, row filter 0, a last row of 0 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[0, 10, 20, 30, 40, 50, 0],
        &[10, 20, 30, 40, 50],
    ),
    // PNG, row filter 0, a last row of 1 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[0, 10, 20, 30, 40, 50, 0, 1],
        &[10, 20, 30, 40, 50, 1],
    ),
    // PNG, row filter 0, a last row of 3 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[0, 10, 20, 30, 40, 50, 0, 1, 2, 3],
        &[10, 20, 30, 40, 50, 1, 2, 3],
    ),
    // PNG, row filter 1, a last row of 0 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[1, 10, 20, 30, 40, 50, 1],
        &[10, 30, 60, 100, 150],
    ),
    // PNG, row filter 1, a last row of 1 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[1, 10, 20, 30, 40, 50, 1, 1],
        &[10, 30, 60, 100, 150, 1],
    ),
    // PNG, row filter 1, a last row of 3 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[1, 10, 20, 30, 40, 50, 1, 1, 2, 3],
        &[10, 30, 60, 100, 150, 1, 3, 6],
    ),
    // PNG, row filter 2, a last row of 0 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[2, 10, 20, 30, 40, 50, 2],
        &[10, 20, 30, 40, 50],
    ),
    // PNG, row filter 2, a last row of 1 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[2, 10, 20, 30, 40, 50, 2, 1],
        &[10, 20, 30, 40, 50, 11],
    ),
    // PNG, row filter 2, a last row of 3 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[2, 10, 20, 30, 40, 50, 2, 1, 2, 3],
        &[10, 20, 30, 40, 50, 11, 22, 33],
    ),
    // PNG, row filter 3, a last row of 0 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[3, 10, 20, 30, 40, 50, 3],
        &[10, 25, 42, 61, 80],
    ),
    // PNG, row filter 3, a last row of 1 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[3, 10, 20, 30, 40, 50, 3, 1],
        &[10, 25, 42, 61, 80, 6],
    ),
    // PNG, row filter 3, a last row of 3 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[3, 10, 20, 30, 40, 50, 3, 1, 2, 3],
        &[10, 25, 42, 61, 80, 6, 17, 32],
    ),
    // PNG, row filter 4, a last row of 0 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[4, 10, 20, 30, 40, 50, 4],
        &[10, 30, 60, 100, 150],
    ),
    // PNG, row filter 4, a last row of 1 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[4, 10, 20, 30, 40, 50, 4, 1],
        &[10, 30, 60, 100, 150, 11],
    ),
    // PNG, row filter 4, a last row of 3 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[4, 10, 20, 30, 40, 50, 4, 1, 2, 3],
        &[10, 30, 60, 100, 150, 11, 32, 63],
    ),
    // PNG, row filter 7, a last row of 0 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[7, 10, 20, 30, 40, 50, 7],
        &[10, 20, 30, 40, 50],
    ),
    // PNG, row filter 7, a last row of 1 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[7, 10, 20, 30, 40, 50, 7, 1],
        &[10, 20, 30, 40, 50, 1],
    ),
    // PNG, row filter 7, a last row of 3 of 5 bytes.
    (
        "/Predictor 12 /Columns 5",
        &[7, 10, 20, 30, 40, 50, 7, 1, 2, 3],
        &[10, 20, 30, 40, 50, 1, 2, 3],
    ),
    // PNG, 3-byte pixels, row filter 1, a last row cut inside a pixel.
    (
        "/Predictor 12 /Colors 3 /Columns 2",
        &[1, 81, 192, 221, 143, 237, 163, 1, 176, 163, 127, 28],
        &[81, 192, 221, 224, 173, 128, 176, 163, 127, 204],
    ),
    // PNG, 3-byte pixels, row filter 3, a last row cut inside a pixel.
    (
        "/Predictor 12 /Colors 3 /Columns 2",
        &[3, 247, 207, 98, 246, 11, 1, 3, 222, 57, 6, 47],
        &[247, 207, 98, 113, 114, 50, 89, 160, 55, 148],
    ),
    // PNG, 3-byte pixels, row filter 4, a last row cut inside a pixel.
    (
        "/Predictor 12 /Colors 3 /Columns 2",
        &[4, 68, 23, 144, 28, 231, 16, 4, 128, 225, 227, 221],
        &[68, 23, 144, 96, 254, 160, 196, 248, 115, 161],
    ),
    // PNG, 5-byte pixels, row filter 1, a last row cut inside a pixel.
    (
        "/Predictor 12 /Colors 5 /Columns 2",
        &[
            1, 81, 192, 221, 143, 237, 163, 66, 47, 105, 197, 2, 162, 132, 177, 192, 144, 228, 104,
            167, 7, 237, 1, 176, 163, 127, 28, 190, 55, 235,
        ],
        &[
            81, 192, 221, 143, 237, 244, 2, 12, 248, 178, 243, 68, 142, 79, 125, 216, 106, 179,
            255, 159, 176, 163, 127, 28, 190, 231, 142,
        ],
    ),
    // PNG, 5-byte pixels, row filter 2, a last row cut inside a pixel.
    (
        "/Predictor 12 /Colors 5 /Columns 2",
        &[
            2, 162, 132, 177, 192, 144, 228, 104, 167, 7, 237, 3, 247, 207, 98, 246, 11, 1, 77,
            247, 228, 235, 2, 237, 237, 159, 98, 91, 176, 22,
        ],
        &[
            162, 132, 177, 192, 144, 228, 104, 167, 7, 237, 72, 17, 186, 86, 83, 151, 137, 167, 18,
            139, 53, 254, 89, 184, 174, 71, 159,
        ],
    ),
    // PNG, 5-byte pixels, row filter 3, a last row cut inside a pixel.
    (
        "/Predictor 12 /Colors 5 /Columns 2",
        &[
            3, 247, 207, 98, 246, 11, 1, 77, 247, 228, 235, 4, 68, 23, 144, 28, 231, 16, 237, 22,
            248, 78, 3, 222, 57, 6, 47, 103, 111, 154,
        ],
        &[
            247, 207, 98, 246, 11, 124, 180, 40, 95, 240, 59, 230, 242, 18, 242, 75, 188, 8, 10,
            64, 251, 172, 127, 56, 224, 18, 78,
        ],
    ),
    // PNG, 5-byte pixels, row filter 4, a last row cut inside a pixel.
    (
        "/Predictor 12 /Colors 5 /Columns 2",
        &[
            4, 68, 23, 144, 28, 231, 16, 237, 22, 248, 78, 0, 216, 90, 153, 23, 100, 71, 51, 255,
            221, 101, 4, 128, 225, 227, 221, 167, 246, 248,
        ],
        &[
            68, 23, 144, 28, 231, 84, 4, 166, 20, 53, 216, 90, 153, 23, 100, 71, 51, 255, 221, 101,
            88, 59, 124, 244, 11, 61, 43,
        ],
    ),
    // PNG, 16-bit components, 6-byte pixels, a last row cut inside a pixel.
    (
        "/Predictor 15 /Colors 3 /BitsPerComponent 16 /Columns 1",
        &[4, 56, 136, 121, 130, 118, 134, 4, 37, 246, 136, 133, 103],
        &[56, 136, 121, 130, 118, 134, 93, 126, 1, 7, 221],
    ),
    // PNG, rows of 5 bytes and 2-byte pixels, row filter 1.
    (
        "/Predictor 12 /Colors 3 /BitsPerComponent 4 /Columns 3",
        &[
            1, 48, 232, 166, 56, 98, 1, 122, 14, 0, 30, 61, 1, 214, 232, 157,
        ],
        &[48, 232, 214, 32, 56, 122, 14, 122, 44, 183, 214, 232, 115],
    ),
    // PNG, rows of 5 bytes and 2-byte pixels, row filter 3.
    (
        "/Predictor 12 /Colors 3 /BitsPerComponent 4 /Columns 3",
        &[
            3, 21, 97, 79, 15, 41, 3, 174, 115, 80, 120, 93, 3, 15, 54, 105,
        ],
        &[21, 97, 89, 63, 85, 184, 163, 216, 233, 243, 107, 135, 10],
    ),
    // PNG, rows of 5 bytes and 2-byte pixels, row filter 4.
    (
        "/Predictor 12 /Colors 3 /BitsPerComponent 4 /Columns 3",
        &[
            4, 180, 149, 65, 114, 38, 4, 113, 121, 99, 182, 83, 4, 74, 225, 33,
        ],
        &[180, 149, 245, 7, 27, 37, 14, 136, 189, 110, 111, 239, 169],
    ),
    // TIFF, 8-bit components, 3 colors, a last row cut inside a pixel.
    (
        "/Predictor 2 /Colors 3 /Columns 2",
        &[87, 137, 138, 38, 189, 63, 149, 100, 41, 6],
        &[87, 137, 138, 125, 70, 201, 149, 100, 41, 155],
    ),
    // TIFF, 8-bit components, 5 colors, a last row cut inside a pixel.
    (
        "/Predictor 2 /Colors 5 /Columns 2",
        &[
            44, 24, 195, 29, 211, 20, 213, 157, 253, 38, 92, 82, 253, 205, 175, 86, 117,
        ],
        &[
            44, 24, 195, 29, 211, 64, 237, 96, 26, 249, 92, 82, 253, 205, 175, 178, 199,
        ],
    ),
    // TIFF, 16-bit components, a last row cut inside a component: its low byte is the one the row before held there, as stored.
    (
        "/Predictor 2 /BitsPerComponent 16 /Columns 3",
        &[10, 20, 30, 40, 50, 60, 1, 2, 3, 132, 53],
        &[10, 20, 40, 60, 90, 120, 1, 2, 4, 134, 57],
    ),
    // TIFF, 16-bit components, 2 colors, a last row cut inside a component.
    (
        "/Predictor 2 /BitsPerComponent 16 /Colors 2 /Columns 2",
        &[
            172, 87, 147, 139, 240, 208, 231, 52, 157, 150, 191, 102, 233, 93, 50, 126, 113, 56,
            94, 33, 35, 42, 117,
        ],
        &[
            172, 87, 147, 139, 157, 39, 122, 191, 157, 150, 191, 102, 134, 243, 241, 228, 113, 56,
            94, 33, 148, 98, 211,
        ],
    ),
    // TIFF, 4-bit components, rows of 11: the bits after the last are cleared.
    (
        "/Predictor 2 /BitsPerComponent 4 /Columns 11",
        &[
            216, 160, 99, 170, 79, 202, 5, 190, 189, 226, 66, 224, 87, 126, 155, 19,
        ],
        &[
            213, 255, 88, 44, 15, 176, 5, 14, 150, 70, 172, 160, 92, 49, 165, 105,
        ],
    ),
    // TIFF, 1-bit components, rows of 45.
    (
        "/Predictor 2 /BitsPerComponent 1 /Columns 45",
        &[134, 8, 5, 129, 237, 40, 74, 220, 123, 251, 98, 136, 96, 213],
        &[
            251, 240, 6, 254, 182, 48, 115, 104, 82, 173, 188, 240, 64, 153,
        ],
    ),
    // TIFF, 2-bit components, 3 colors, rows of 18 bits.
    (
        "/Predictor 2 /BitsPerComponent 2 /Colors 3 /Columns 3",
        &[107, 103, 197, 105, 133, 168, 118, 26],
        &[104, 198, 192, 106, 45, 0, 119, 229],
    ),
    // An unknown predictor passes the data on.
    (
        "/Predictor 3 /Columns 5",
        &[43, 213, 86, 181, 88, 120, 5, 114, 209, 27, 185, 1, 144],
        &[43, 213, 86, 181, 88, 120, 5, 114, 209, 27, 185, 1, 144],
    ),
    // A predictor above 15 passes the data on.
    (
        "/Predictor 16 /Columns 5",
        &[120, 215, 31, 152, 129, 14, 151, 99, 130, 219, 105, 10, 123],
        &[120, 215, 31, 152, 129, 14, 151, 99, 130, 219, 105, 10, 123],
    ),
    // Colors, bits per component and columns of zero are their defaults.
    (
        "/Predictor 12 /Colors 0 /BitsPerComponent 0 /Columns 0",
        &[1, 5, 1, 7, 1],
        &[5, 7],
    ),
    // A real is rounded: 5.5 columns are 6.
    (
        "/Predictor 12 /Columns 5.5",
        &[1, 104, 39, 178, 12, 93, 208, 1, 59, 36, 9],
        &[104, 143, 65, 77, 170, 122, 59, 95, 104],
    ),
    // An integer is read by its low 32 bits: 4294967301 columns are 5.
    (
        "/Predictor 12 /Columns 4294967301",
        &[1, 86, 216, 225, 233, 55, 1, 133, 18, 157],
        &[86, 46, 15, 248, 47, 133, 151, 52],
    ),
    // A real's integer part is read by its low 32 bits: 4294967308.0 is predictor 12.
    (
        "/Predictor 4294967308.0 /Columns 5",
        &[1, 191, 52, 127, 146, 195, 1, 13, 249, 254],
        &[191, 243, 114, 4, 199, 13, 6, 4],
    ),
    // A row far longer than the data is a first row cut short.
    (
        "/Predictor 12 /Columns 100000000",
        &[1, 125, 243, 136, 28, 230, 25, 77, 184, 94, 254, 221, 108],
        &[125, 112, 248, 20, 250, 19, 96, 24, 118, 116, 81, 189],
    ),
];

/// Each case decodes to `MuPDF`'s bytes, compressed by Flate under `/DecodeParms`
/// or `/DP` and by LZW, read whole and a piece at a time.
#[test]
fn predicted_rows_decode_as_mupdf_decodes_them() {
    for (parms, rows, expected) in CASES {
        for (filter, encoded) in [("/FlateDecode", zlib(rows)), ("/LZWDecode", lzw(rows))] {
            for key in ["/DecodeParms", "/DP"] {
                let dict = format!("/Filter {filter} {key} << {parms} >>");
                let pdf = stream_file(&dict, &encoded);
                for (how, read) in every_reading(&object_3(&pdf)) {
                    assert_eq!(read.as_deref(), Some(*expected), "{dict}: {how}");
                }
            }
        }
    }
}

/// Parameters `MuPDF` refuses fail decoding, as there: more than 32 colors, bits per
/// component other than 1, 2, 4, 8 and 16, columns whose row's bits come near 2^31,
/// and a predictor whose reading is not established. Under no predictor the others
/// are not read.
#[test]
fn predictor_parameters_mupdf_refuses_fail_decoding() {
    let rows = [2, 1, 2, 3, 4, 5, 2, 6];
    let decoded = |parms: &str| {
        let dict = format!("/Filter /FlateDecode /DecodeParms << {parms} >>");
        let pdf = stream_file(&dict, &zlib(&rows));
        let readings = every_reading(&object_3(&pdf));
        (dict, readings)
    };
    for (predictor, rest) in [
        ("12", "/Colors 33"),
        ("2", "/Colors 255"),
        ("12", "/BitsPerComponent 3"),
        ("2", "/BitsPerComponent 32"),
        ("12", "/Columns 268435455"),
        ("2", "/Colors 3 /BitsPerComponent 4 /Columns 178956970"),
        ("99999999999999999999", ""),
    ] {
        let (dict, readings) = decoded(&format!("/Predictor {predictor} {rest}"));
        for (how, read) in readings {
            assert_eq!(read, None, "{dict}: {how}");
        }
        let (dict, readings) = decoded(&format!("/Predictor 1 {rest}"));
        for (how, read) in readings {
            assert_eq!(read.as_deref(), Some(&rows[..]), "{dict}: {how}");
        }
    }
}

/// Data cut short keep every byte they hold, by every way of decoding, where
/// `PyMuPDF` 1.28.2 reads otherwise: an odd final hex digit is padded with 0 (as the
/// PDF specification says; `MuPDF` drops it), ASCII85 without `~>` keeps its last
/// group (`MuPDF` reads 24 of these 26 bytes), and LZW codes cut short within a code
/// give the codes read whole (`MuPDF` appends a byte 0xFF for the leftover bits, which
/// in a page content turns a last `f` into `f\xff` and loses the fill).
#[test]
fn data_cut_short_in_other_filters_keep_what_they_hold() {
    let body = b"HELLO ABCDEFGHIJKLMNOPQRST";
    let hex: String = body.iter().map(|b| format!("{b:02x}")).collect();
    let lzw_cut = {
        let full = lzw(body);
        full[..full.len() - 2].to_vec()
    };
    for (filter, data, expected) in [
        (
            "/AHx",
            hex.as_bytes()[..hex.len() - 1].to_vec(),
            [&body[..25], b"P"].concat(),
        ),
        (
            "/A85",
            b"84?3G:EVqi6UX@47n?3D92&&T:Jand;cE".to_vec(),
            body.to_vec(),
        ),
        ("/LZW", lzw_cut, body[..25].to_vec()),
    ] {
        let dict = format!("/Filter {filter}");
        let pdf = stream_file(&dict, &data);
        for (how, read) in every_reading(&object_3(&pdf)) {
            assert_eq!(read.as_deref(), Some(&expected[..]), "{dict}: {how}");
        }
    }
}

/// What a filter list decodes to (see [`FILTER_CASES`]).
#[derive(Clone, Copy, Debug)]
enum Data {
    /// [`ROWS`] compressed with zlib.
    Flate,
    /// The zlib data in hexadecimal.
    Hex,
}

/// Two rows of five bytes under PNG filter 0.
const ROWS: &[u8] = b"\x00HELLO\x00WORLD";

/// What `PyMuPDF` decodes a case to.
#[derive(Clone, Copy, Debug)]
enum Out {
    /// [`ROWS`]: Flate applied without the predictor.
    Rows,
    /// The rows with the predictor reversed.
    Predicted,
    /// The data as stored: no filter applied.
    Stored,
}

/// A stream's entries (object 3), its data, objects 4 on, and what `PyMuPDF` 1.28.2
/// decodes it to (`Document::xref_stream`).
#[rustfmt::skip]
const FILTER_CASES: &[(&str, Data, &[&str], Out)] = &[
    ("/Filter [/FlateDecode]", Data::Flate, &[], Out::Rows),
    ("/Filter [null /FlateDecode]", Data::Flate, &[], Out::Rows),
    ("/Filter [/FlateDecode null]", Data::Flate, &[], Out::Rows),
    ("/Filter [1 /FlateDecode]", Data::Flate, &[], Out::Rows),
    ("/Filter [(AHx) /FlateDecode]", Data::Flate, &[], Out::Rows),
    ("/Filter [/Foo /FlateDecode]", Data::Flate, &[], Out::Rows),
    ("/Filter [/FlateDecode /Foo]", Data::Flate, &[], Out::Rows),
    ("/Filter [[/FlateDecode]]", Data::Flate, &[], Out::Stored),
    ("/Filter [4 0 R]", Data::Flate, &["/FlateDecode"], Out::Rows),
    ("/Filter [8 0 R /FlateDecode]", Data::Flate, &[], Out::Rows),
    ("/Filter [4 0 R /FlateDecode]", Data::Flate, &["null"], Out::Rows),
    ("/Filter [/Crypt /FlateDecode]", Data::Flate, &[], Out::Rows),
    ("/Filter [null /Crypt /FlateDecode]", Data::Flate, &[], Out::Rows),
    ("/Filter [/AHx null /Fl]", Data::Hex, &[], Out::Rows),
    ("/Filter 4 0 R", Data::Flate, &["[/FlateDecode]"], Out::Rows),
    ("/Filter 5", Data::Flate, &[], Out::Stored),
    ("/Filter /Foo", Data::Flate, &[], Out::Stored),
    ("/Filter [/Foo]", Data::Flate, &[], Out::Stored),
    ("/Filter [/FlateDecode] /DecodeParms [<< /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Predicted),
    ("/Filter [/FlateDecode] /DecodeParms << /Predictor 12 /Columns 5 >>", Data::Flate, &[], Out::Rows),
    ("/Filter /FlateDecode /DecodeParms [<< /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Rows),
    ("/Filter /FlateDecode /DecodeParms null", Data::Flate, &[], Out::Rows),
    ("/Filter [null /FlateDecode] /DecodeParms [null << /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Predicted),
    ("/Filter [null /FlateDecode] /DecodeParms [<< /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Rows),
    ("/Filter [/Foo /FlateDecode] /DecodeParms [null << /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Predicted),
    ("/Filter [/Foo /FlateDecode] /DecodeParms [<< /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Rows),
    ("/Filter [1 /FlateDecode] /DecodeParms [null << /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Predicted),
    ("/Filter [/AHx /Fl] /DecodeParms [1 << /Predictor 12 /Columns 5 >>]", Data::Hex, &[], Out::Predicted),
    ("/Filter [/AHx /Fl] /DecodeParms [<< /Predictor 12 /Columns 5 >>]", Data::Hex, &[], Out::Rows),
    ("/Filter [/AHx /Fl] /DecodeParms [null 4 0 R]", Data::Hex, &["<< /Predictor 12 /Columns 5 >>"], Out::Predicted),
    ("/Filter [/AHx /Fl] /DecodeParms [8 0 R << /Predictor 12 /Columns 5 >>]", Data::Hex, &[], Out::Predicted),
    ("/Filter [/AHx /Fl] /DecodeParms [[] << /Predictor 12 /Columns 5 >>]", Data::Hex, &[], Out::Predicted),
    ("/Filter [/FlateDecode] /DecodeParms 4 0 R", Data::Flate, &["[<< /Predictor 12 /Columns 5 >>]"], Out::Predicted),
    ("/Filter [/FlateDecode] /DecodeParms 4 0 R", Data::Flate, &["[5 0 R]", "<< /Predictor 12 /Columns 5 >>"], Out::Predicted),
    ("/Filter 4 0 R /DecodeParms << /Predictor 12 /Columns 5 >>", Data::Flate, &["/FlateDecode"], Out::Predicted),
    ("/Filter [null /Crypt /FlateDecode] /DP [null null << /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Predicted),
    ("/Filter [null /FlateDecode] /DP [null null << /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Rows),
    ("/Filter /FlateDecode /DP << /Predictor 12 /Columns 5 >> /DecodeParms << /Predictor 1 >>", Data::Flate, &[], Out::Predicted),
    ("/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 5 >> /DP << /Predictor 1 >>", Data::Flate, &[], Out::Rows),
    ("/Filter /FlateDecode /DP 5 /DecodeParms << /Predictor 12 /Columns 5 >>", Data::Flate, &[], Out::Rows),
    ("/Filter /FlateDecode /DP (x) /DecodeParms << /Predictor 12 /Columns 5 >>", Data::Flate, &[], Out::Rows),
    ("/Filter /FlateDecode /DP null /DecodeParms << /Predictor 12 /Columns 5 >>", Data::Flate, &[], Out::Predicted),
    ("/Filter /FlateDecode /DP 8 0 R /DecodeParms << /Predictor 12 /Columns 5 >>", Data::Flate, &[], Out::Rows),
    ("/Filter /FlateDecode /DP 4 0 R /DecodeParms << /Predictor 12 /Columns 5 >>", Data::Flate, &["null"], Out::Rows),
    ("/Filter /FlateDecode /DP 4 0 R /DecodeParms << /Predictor 1 >>", Data::Flate, &["<< /Predictor 12 /Columns 5 >>"], Out::Predicted),
    ("/Filter /FlateDecode /DP [<< /Predictor 12 /Columns 5 >>] /DecodeParms << /Predictor 1 >>", Data::Flate, &[], Out::Rows),
    ("/Filter [/FlateDecode] /DP [null] /DecodeParms [<< /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Rows),
    ("/Filter [/FlateDecode] /DP << /Predictor 12 /Columns 5 >> /DecodeParms [<< /Predictor 1 >>]", Data::Flate, &[], Out::Rows),
    ("/Filter [/FlateDecode] /DP 5 /DecodeParms [<< /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Rows),
    ("/Filter [/FlateDecode] /DP null /DecodeParms [<< /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Predicted),
    ("/Filter [/FlateDecode] /DP 8 0 R /DecodeParms [<< /Predictor 12 /Columns 5 >>]", Data::Flate, &[], Out::Rows),
    ("/Filter [/FlateDecode] /DecodeParms [<< /Predictor 12 /Columns 5 >>] /DP []", Data::Flate, &[], Out::Rows),
];

#[test]
fn filters_keep_their_parameters_in_place_as_mupdf_reads_them() {
    let zlib_rows = zlib(ROWS);
    let mut differ = Vec::new();
    for &(dict, data, extra, expected) in FILTER_CASES {
        let data = match data {
            Data::Flate => zlib_rows.clone(),
            Data::Hex => {
                let mut hex: Vec<u8> = zlib_rows
                    .iter()
                    .flat_map(|b| format!("{b:02x}").into_bytes())
                    .collect();
                hex.push(b'>');
                hex
            }
        };
        let expected = match expected {
            Out::Rows => ROWS,
            Out::Predicted => b"HELLOWORLD",
            Out::Stored => &data,
        };
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            common::stream(dict, &data),
        ];
        objects.extend(extra.iter().map(|e| e.as_bytes().to_vec()));
        let pdf = Pdf::new(common::file(&objects, "")).unwrap();
        for (reading, got) in every_reading(&object_3(&pdf)) {
            if got.as_deref() != Some(expected) {
                differ.push(format!("{dict} {extra:?}: {reading} gives {got:?}"));
            }
        }
    }
    assert!(differ.is_empty(), "{differ:#?}");
}
