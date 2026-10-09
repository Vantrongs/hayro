//! Resource and byte-parity regressions at the public stream reader boundary.

use hayro_syntax::object::Stream;
use hayro_syntax::reader::{Reader, ReaderContext, ReaderExt};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct MeasuredAllocator;
thread_local! {
    static LARGEST: Cell<Option<usize>> = const { Cell::new(None) };
}

fn record(size: usize) {
    LARGEST.with(|largest| {
        if let Some(previous) = largest.get() {
            largest.set(Some(previous.max(size)));
        }
    });
}

unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

fn largest_allocation(f: impl FnOnce()) -> usize {
    LARGEST.with(|largest| largest.set(Some(0)));
    f();
    LARGEST.with(|largest| largest.replace(None).unwrap())
}

#[test]
fn unavailable_predictor_rows_do_not_allocate_the_declared_width() {
    for columns in [1 << 20, usize::MAX / 32] {
        for input in [&[][..], &[0, 1, 2, 3][..]] {
            // A single uncompressed zlib block, so no compressor is needed even
            // when testing the no_std-compatible decoder.
            let n = input.len() as u16;
            let mut encoded = vec![0x78, 1, 1];
            encoded.extend(n.to_le_bytes());
            encoded.extend((!n).to_le_bytes());
            encoded.extend(input);
            let (mut a, mut b) = (1_u32, 0_u32);
            for &byte in input {
                a += u32::from(byte);
                b += a;
            }
            encoded.extend(((b << 16) | a).to_be_bytes());
            let mut source = format!(
                "<< /Length {} /Filter /FlateDecode /DecodeParms << /Predictor 12 /Colors 1 /BitsPerComponent 8 /Columns {columns} >> >> stream\n",
                encoded.len(),
            ).into_bytes();
            source.extend(encoded);
            source.extend(b"\nendstream");
            let stream = Reader::new(&source)
                .read_with_context::<Stream<'_>>(&ReaderContext::dummy())
                .unwrap();
            // Parsing DecodeParms builds a small dictionary; no decoder, source
            // data or predictor row may be allocated by this metadata probe.
            assert!(largest_allocation(|| assert!(stream.can_read_incrementally())) < 4096);
            let allocation = largest_allocation(|| {
                let mut reader = stream.decoded_reader().unwrap();
                assert_eq!(reader.read(&mut [0; 4]), Ok(0));
                reader.rewind();
                assert_eq!(reader.read(&mut [0; 4]), Ok(0));
            });
            assert!(
                allocation < 128 * 1024,
                "allocated {allocation} bytes for {} row bytes",
                input.len()
            );
        }
    }
}

fn encrypted_pdf(data: &[u8], filter: &str) -> hayro_syntax::Pdf {
    encrypted_pdf_with_keys(data, filter, [7; 32], [7; 32])
}

fn encrypted_pdf_with_keys(
    data: &[u8],
    filter: &str,
    key: [u8; 32],
    payload_key: [u8; 32],
) -> hayro_syntax::Pdf {
    use aes::cipher::{
        BlockEncryptMut, KeyIvInit,
        block_padding::{NoPadding, Pkcs7},
    };
    use sha2::{Digest, Sha256};
    type Encryptor = cbc::Encryptor<aes::Aes256>;
    let salt = [3; 8];
    let hash = Sha256::digest(salt);
    let user: Vec<u8> = hash.iter().copied().chain(salt).chain(salt).collect();
    let mut ue = key;
    Encryptor::new_from_slices(&hash, &[0; 16])
        .unwrap()
        .encrypt_padded_mut::<NoPadding>(&mut ue, 32)
        .unwrap();
    let mut padded = vec![0; data.len() + 16];
    padded[..data.len()].copy_from_slice(data);
    let encrypted = Encryptor::new_from_slices(&payload_key, &[9; 16])
        .unwrap()
        .encrypt_padded_mut::<Pkcs7>(&mut padded, data.len())
        .unwrap();
    let ciphertext: Vec<u8> = [9; 16]
        .into_iter()
        .chain(encrypted.iter().copied())
        .collect();
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut stream = format!("<< /Length {} {filter} >>\nstream\n", ciphertext.len()).into_bytes();
    stream.extend(ciphertext);
    stream.extend(b"\nendstream");
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
        stream,
        format!("<< /Filter /Standard /V 5 /R 5 /Length 256 /P -4 /O <{}> /U <{}> /UE <{}> /StmF /StdCF /StrF /StdCF /CF << /StdCF << /CFM /AESV3 /Length 32 >> >> >>", hex(&[0; 48]), hex(&user), hex(&ue)).into_bytes(),
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend(format!("{} 0 obj\n", i + 1).bytes());
        pdf.extend(object);
        pdf.extend(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets {
        pdf.extend(format!("{offset:010} 00000 n \n").bytes());
    }
    pdf.extend(format!("trailer\n<< /Size 5 /Root 1 0 R /Encrypt 4 0 R /ID [<01> <01>] >>\nstartxref\n{xref}\n%%EOF\n").bytes());
    hayro_syntax::Pdf::new(pdf).unwrap()
}

#[test]
fn content_cache_hash_accounts_for_the_decryption_context_without_decrypting() {
    use hayro_syntax::object::ObjectIdentifier;
    use std::hash::{DefaultHasher, Hasher};

    for length in [65_536, 262_144] {
        let data = vec![71; length];
        let first = encrypted_pdf_with_keys(&data, "", [7; 32], [7; 32]);
        // Identical stored stream bytes, but a different file decryption key.
        let second = encrypted_pdf_with_keys(&data, "", [8; 32], [7; 32]);
        fn stream(pdf: &hayro_syntax::Pdf) -> Stream<'_> {
            pdf.xref()
                .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
                .unwrap()
        }
        let first_stream = stream(&first);
        let second_stream = stream(&second);
        assert_eq!(first_stream, second_stream);
        assert_ne!(first_stream.raw_data(), second_stream.raw_data());
        let hash = |stream: &Stream<'_>| {
            let mut state = DefaultHasher::new();
            stream.hash_content(&mut state);
            state.finish()
        };
        assert_ne!(hash(&first_stream), hash(&second_stream));
        let allocation = largest_allocation(|| {
            assert_eq!(hash(&first_stream), hash(&stream(&first)));
        });
        assert!(allocation < 4096, "hash allocated {allocation} bytes");
    }
}

#[test]
fn encrypted_raw_readers_do_not_retain_the_whole_decrypted_stream() {
    use hayro_syntax::object::ObjectIdentifier;
    for length in [65_536, 262_144] {
        let data: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
        let pdf = encrypted_pdf(&data, "");
        let stream = pdf
            .xref()
            .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
            .unwrap();
        assert_eq!(
            largest_allocation(|| assert!(stream.can_read_incrementally())),
            0
        );
        let allocation = largest_allocation(|| {
            let mut reader = stream.decoded_reader().unwrap();
            let mut prefix = [0; 37];
            let mut filled = 0;
            while filled < prefix.len() {
                filled += reader.read(&mut prefix[filled..]).unwrap();
            }
            assert_eq!(prefix, data[..37]);
            reader.rewind();
            assert_eq!(reader.read(&mut prefix[..1]), Ok(1));
            assert_eq!(prefix[0], data[0]);
        });
        assert!(
            allocation < 32 * 1024,
            "allocated {allocation} bytes for {length} source bytes"
        );
    }
}

#[cfg(feature = "unsafe")]
#[test]
fn encrypted_flate_restarts_and_rewinds_from_ciphertext() {
    use hayro_syntax::object::ObjectIdentifier;
    use hayro_syntax::object::stream::ReadError;
    use std::io::Write;
    let data: Vec<u8> = (0..32_769_u32).map(|i| ((i / 7) % 251) as u8).collect();
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&data).unwrap();
    let zlib = encoder.finish().unwrap();
    for encoded in [&zlib[..], &zlib[2..zlib.len() - 4]] {
        let pdf = encrypted_pdf(encoded, "/Filter /FlateDecode");
        let stream = pdf
            .xref()
            .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
            .unwrap();
        assert_eq!(
            largest_allocation(|| assert!(stream.can_read_incrementally())),
            0
        );
        let mut reader = stream.decoded_reader().unwrap();
        let mut buf = [0; 397];
        let mut restarts = 0;
        for pass in 0..2 {
            let mut actual = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => actual.extend_from_slice(&buf[..n]),
                    Err(ReadError::Restarted) => {
                        assert_eq!(pass, 0, "rewind repeated decoder selection");
                        restarts += 1;
                        actual.clear();
                    }
                    Err(ReadError::Failed) => panic!("failed to read encrypted Flate"),
                }
            }
            assert_eq!(actual, data);
            reader.rewind();
        }
        assert_eq!(restarts, usize::from(encoded.len() != zlib.len()));
    }
}

#[cfg(feature = "unsafe")]
#[test]
fn encrypted_corrupt_flate_uses_the_same_permissive_fallback() {
    use hayro_syntax::object::ObjectIdentifier;
    use hayro_syntax::object::stream::ReadError;
    // One valid, non-final stored block, followed by a reserved block type.
    let mut encoded = vec![0x78, 1, 0, 0x88, 0x13, 0x77, 0xec];
    encoded.extend((0..5000).map(|i| (i % 251) as u8));
    encoded.push(7);
    let pdf = encrypted_pdf(&encoded, "/Filter /FlateDecode");
    let stream = pdf
        .xref()
        .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
        .unwrap();
    let expected = stream.decoded().unwrap();
    let mut reader = stream.decoded_reader().unwrap();
    let mut buf = [0; 397];
    let mut restarts = 0;
    for pass in 0..2 {
        let mut actual = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => actual.extend_from_slice(&buf[..n]),
                Err(ReadError::Restarted) => {
                    assert_eq!(pass, 0);
                    restarts += 1;
                    actual.clear();
                }
                Err(ReadError::Failed) => panic!("permissive decode failed"),
            }
        }
        assert_eq!(actual, expected.as_ref());
        reader.rewind();
    }
    assert_eq!(restarts, 2);
}
