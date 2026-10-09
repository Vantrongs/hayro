//! Byte, ownership and allocation contracts of the public page content cache.

use hayro_syntax::{Pdf, object::Stream};
use std::alloc::{GlobalAlloc, Layout, System};
use std::borrow::Cow;
use std::cell::Cell;

#[derive(Clone, Copy, Default, Debug)]
struct Allocation {
    live: isize,
    peak: usize,
    count: usize,
}

thread_local! {
    static MEASURE: Cell<Option<Allocation>> = const { Cell::new(None) };
}

fn record(allocated: usize, freed: usize) {
    MEASURE.with(|slot| {
        if let Some(mut value) = slot.get() {
            value.live += allocated as isize - freed as isize;
            value.peak = value.peak.max(value.live.max(0) as usize);
            value.count += usize::from(allocated != 0);
            slot.set(Some(value));
        }
    });
}

struct MeasuredAllocator;
unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size(), 0);
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record(layout.size(), 0);
        }
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(ptr, layout, size) };
        if !result.is_null() {
            record(size, layout.size());
        }
        result
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(0, layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

fn measured<T>(f: impl FnOnce() -> T) -> (T, Allocation) {
    MEASURE.with(|slot| slot.set(Some(Allocation::default())));
    let result = f();
    let allocation = MEASURE.with(|slot| slot.replace(None).unwrap());
    (result, allocation)
}

fn stream(data: &[u8], filter: &str) -> Vec<u8> {
    let mut result = format!("<< /Length {} {filter} >>\nstream\n", data.len()).into_bytes();
    result.extend(data);
    result.extend(b"\nendstream");
    result
}

fn pdf(contents: &str, mut streams: Vec<Vec<u8>>, encryption: Option<Vec<u8>>) -> Pdf {
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] {contents} >>").into_bytes(),
    ];
    objects.append(&mut streams);
    let encrypt = if let Some(dictionary) = encryption {
        objects.push(dictionary);
        format!("/Encrypt {} 0 R /ID [<01> <01>]", objects.len())
    } else {
        String::new()
    };
    let mut result = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(result.len());
        result.extend(format!("{} 0 obj\n", i + 1).bytes());
        result.extend(object);
        result.extend(b"\nendobj\n");
    }
    let xref = result.len();
    let size = objects.len() + 1;
    result.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").bytes());
    for offset in offsets {
        result.extend(format!("{offset:010} 00000 n \n").bytes());
    }
    result.extend(
        format!("trailer\n<< /Size {size} /Root 1 0 R {encrypt} >>\nstartxref\n{xref}\n%%EOF\n")
            .bytes(),
    );
    Pdf::new(result).unwrap()
}

// Stored zlib blocks exercise both decoders without a compressor dependency.
fn flate(data: &[u8]) -> Vec<u8> {
    let mut result = vec![0x78, 1];
    let mut chunks = data.chunks(u16::MAX as usize).peekable();
    while let Some(chunk) = chunks.next() {
        result.push(u8::from(chunks.peek().is_none()));
        let len = chunk.len() as u16;
        result.extend(len.to_le_bytes());
        result.extend((!len).to_le_bytes());
        result.extend(chunk);
    }
    let (mut a, mut b) = (1_u32, 0_u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    result.extend(((b << 16) | a).to_be_bytes());
    result
}

fn assert_cached(pdf: &Pdf, expected: Option<&[u8]>) {
    let page = &pdf.pages()[0];
    let first = page.page_stream();
    assert_eq!(first, expected);
    let (second, allocation) = measured(|| page.page_stream());
    assert_eq!(second.map(<[u8]>::as_ptr), first.map(<[u8]>::as_ptr));
    assert_eq!(
        allocation.count, 0,
        "cached content allocated: {allocation:?}"
    );
}

#[test]
fn decoded_page_keeps_one_content_buffer() {
    let data = vec![b' '; (1 << 20) - 16];
    let pdf = pdf(
        "/Contents 4 0 R",
        vec![stream(&flate(&data), "/Filter /FlateDecode")],
        None,
    );
    let page = &pdf.pages()[0];
    let (actual, allocation) = measured(|| page.page_stream().unwrap());
    assert_eq!(actual, data);
    // Decoder capacity and small dictionaries fit; a second content-sized copy does not.
    let ceiling = data.len() * 3 / 2 + 128 * 1024;
    assert!(
        allocation.peak < ceiling,
        "page content peak {allocation:?}, ceiling {ceiling}"
    );
    assert_cached(&pdf, Some(&data));
}

#[test]
fn borrowed_and_filtered_content_preserve_bytes_and_cache() {
    let data = b"0 0 m 1 1 l S";
    for (encoded, filter, owned) in [
        (data.to_vec(), "", false),
        (flate(data), "/Filter /FlateDecode", true),
        (
            flate(&[b"\0".as_slice(), data].concat()),
            "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 13 >>",
            true,
        ),
    ] {
        let pdf = pdf("/Contents 4 0 R", vec![stream(&encoded, filter)], None);
        let source = pdf.pages()[0].raw().get::<Stream<'_>>(b"Contents").unwrap();
        let decoded = source.decoded().unwrap();
        assert_eq!(matches!(decoded, Cow::Owned(_)), owned);
        assert_eq!(decoded.as_ref(), data);
        assert_cached(&pdf, Some(data));
        if !owned {
            assert_ne!(
                pdf.pages()[0].page_stream().unwrap().as_ptr(),
                decoded.as_ptr()
            );
        }
    }
}

#[test]
fn arrays_keep_order_separators_and_skip_decode_errors() {
    for (contents, expected) in [
        ("/Contents [4 0 R]", Some(b"0 0 m ".as_slice())),
        (
            "/Contents [4 0 R 5 0 R 6 0 R]",
            Some(b"0 0 m 1 1 l S ".as_slice()),
        ),
        // Typed array iteration ends at a non-stream entry.
        ("/Contents [4 0 R null 6 0 R]", Some(b"0 0 m ".as_slice())),
        ("/Contents [5 0 R]", Some(b"".as_slice())),
        ("/Contents []", Some(b"".as_slice())),
        ("/Contents 5 0 R", None),
        ("", None),
        ("/Contents 42", None),
    ] {
        let pdf = pdf(
            contents,
            vec![
                stream(b"0 0 m", ""),
                stream(b"z", "/Filter /ASCIIHexDecode"),
                stream(&flate(b"1 1 l S"), "/Filter /FlateDecode"),
            ],
            None,
        );
        assert_cached(&pdf, expected);
    }
}

#[test]
fn decrypted_content_transfers_ownership_with_or_without_filters() {
    use aes::cipher::{
        BlockEncryptMut, KeyIvInit,
        block_padding::{NoPadding, Pkcs7},
    };
    use sha2::{Digest, Sha256};
    type Encryptor = cbc::Encryptor<aes::Aes256>;
    let data = b"0 0 m 1 1 l S";
    let key = [7; 32];
    let salt = [3; 8];
    let hash = Sha256::digest(salt);
    let user: Vec<u8> = hash.iter().copied().chain(salt).chain(salt).collect();
    let mut ue = key;
    Encryptor::new_from_slices(&hash, &[0; 16])
        .unwrap()
        .encrypt_padded_mut::<NoPadding>(&mut ue, 32)
        .unwrap();
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let encryption = format!("<< /Filter /Standard /V 5 /R 5 /Length 256 /P -4 /O <{}> /U <{}> /UE <{}> /StmF /StdCF /StrF /StdCF /CF << /StdCF << /CFM /AESV3 /Length 32 >> >> >>", hex(&[0; 48]), hex(&user), hex(&ue)).into_bytes();
    for (encoded, filter) in [(data.to_vec(), ""), (flate(data), "/Filter /FlateDecode")] {
        let mut padded = vec![0; encoded.len() + 16];
        padded[..encoded.len()].copy_from_slice(&encoded);
        let encrypted = Encryptor::new_from_slices(&key, &[9; 16])
            .unwrap()
            .encrypt_padded_mut::<Pkcs7>(&mut padded, encoded.len())
            .unwrap();
        let ciphertext: Vec<u8> = [9; 16]
            .into_iter()
            .chain(encrypted.iter().copied())
            .collect();
        let pdf = pdf(
            "/Contents 4 0 R",
            vec![stream(&ciphertext, filter)],
            Some(encryption.clone()),
        );
        let source = pdf.pages()[0].raw().get::<Stream<'_>>(b"Contents").unwrap();
        assert!(matches!(source.decoded().unwrap(), Cow::Owned(_)));
        assert_cached(&pdf, Some(data));
    }
}
