//! Files encrypted with 40-bit RC4 (`/V 1 /R 2`) and the empty passwords, whose strings
//! and streams decrypt with a key of their object's number: the same bytes read as
//! another object decrypt to other bytes.

/// An object of a test file.
pub(crate) enum Part<'a> {
    /// An object written as it is, with `STRING` replaced by `string`, encrypted.
    Object(&'a str, &'a [u8]),
    /// A stream holding `data`, encrypted.
    Stream(&'a [u8]),
    /// A stream holding `data`, encrypted, its dictionary holding `entries` too.
    #[allow(dead_code)]
    StreamWith(&'a str, &'a [u8]),
}

const PADDING: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41, 0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08,
    0x2e, 0x2e, 0x00, 0xb6, 0xd0, 0x68, 0x3e, 0x80, 0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];
const ID: &[u8] = b"rc4-test-file!!!";
const PERMISSIONS: i32 = -4;

pub(crate) fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: Vec<u8> = (0..=255).collect();
    let mut j = 0_u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0_u8, 0_u8);
    data.iter()
        .map(|b| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            b ^ s[s[i as usize].wrapping_add(s[j as usize]) as usize]
        })
        .collect()
}

/// The 40-bit file key, and the /O and /U entries.
fn keys() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    // Algorithm 3: /O from the empty owner password.
    let owner = rc4(&md5::compute(PADDING).0[..5], &PADDING);
    // Algorithm 2: the file key.
    let mut input = PADDING.to_vec();
    input.extend(&owner);
    input.extend(PERMISSIONS.to_le_bytes());
    input.extend(ID);
    let key = md5::compute(&input).0[..5].to_vec();
    // Algorithm 4: /U.
    let user = rc4(&key, &PADDING);
    (key, owner, user)
}

/// `data` encrypted, or decrypted, as object `number`'s.
pub(crate) fn crypt(number: u32, data: &[u8]) -> Vec<u8> {
    let mut input = keys().0;
    input.extend(&number.to_le_bytes()[..3]);
    input.extend([0, 0]);
    rc4(&md5::compute(&input).0[..10], data)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A file of `objects`, numbered from 1, the first being the catalog, whose table also
/// lists each number `alias` of `aliases` at the offset of object `target`.
pub(crate) fn file(objects: &[Part<'_>], aliases: &[(u32, u32)]) -> Vec<u8> {
    file_with(objects, aliases, true)
}

/// [`file`], or where not `encrypted`, the same file with nothing encrypted.
pub(crate) fn file_with(objects: &[Part<'_>], aliases: &[(u32, u32)], encrypted: bool) -> Vec<u8> {
    file_headed(objects, aliases, encrypted, &|number| {
        format!("{number} 0 obj")
    })
}

/// [`file_with`], each object headed by what `header` makes of its number.
#[allow(dead_code)]
pub(crate) fn file_headed(
    objects: &[Part<'_>],
    aliases: &[(u32, u32)],
    encrypted: bool,
    header: &dyn Fn(u32) -> String,
) -> Vec<u8> {
    let crypt = |number: u32, data: &[u8]| {
        if encrypted {
            crypt(number, data)
        } else {
            data.to_vec()
        }
    };
    let (_, owner, user) = keys();
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![];
    for (i, part) in objects.iter().enumerate() {
        let number = i as u32 + 1;
        offsets.push(out.len());
        out.extend(format!("{}\n", header(number)).bytes());
        match part {
            Part::Object(object, string) => out.extend(
                object
                    .replace("STRING", &format!("<{}>", hex(&crypt(number, string))))
                    .bytes(),
            ),
            Part::Stream(data) | Part::StreamWith(_, data) => {
                let entries = match part {
                    Part::StreamWith(entries, _) => format!("{entries} "),
                    _ => String::new(),
                };
                let data = crypt(number, data);
                out.extend(format!("<< {entries}/Length {} >>\nstream\n", data.len()).bytes());
                out.extend(data);
                out.extend(b"\nendstream");
            }
        }
        out.extend(b"\nendobj\n");
    }
    let size = 1 + aliases
        .iter()
        .map(|(alias, _)| *alias as usize)
        .chain([objects.len()])
        .max()
        .unwrap();
    let xref = out.len();
    out.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").bytes());
    for number in 1..size as u32 {
        let target = aliases
            .iter()
            .find(|(alias, _)| *alias == number)
            .map_or(number, |(_, target)| *target);
        match offsets.get(target as usize - 1) {
            Some(offset) => out.extend(format!("{offset:010} 00000 n \n").bytes()),
            None => out.extend(b"0000000000 65535 f \n"),
        }
    }
    let id = hex(ID);
    let encryption = if encrypted {
        format!(
            "/Encrypt << /Filter /Standard /V 1 /R 2 /P {PERMISSIONS} /O <{}> /U <{}> >>",
            hex(&owner),
            hex(&user)
        )
    } else {
        String::new()
    };
    out.extend(
        format!(
            "trailer\n<< /Size {size} /Root 1 0 R /ID [<{id}> <{id}>] {encryption} >>\n\
             startxref\n{xref}\n%%EOF\n"
        )
        .bytes(),
    );
    out
}

pub(crate) const PALETTE: &[u8] = b"\xff\x00\x00";
const RESOURCES: &str = "<< /ColorSpace << /CS [/Indexed /DeviceRGB 0 STRING] >> >>";
const COLOR_SPACES: &str = "<< /CS [/Indexed /DeviceRGB 0 STRING] >>";

/// A file of two pages drawn with color space `/CS` of a one-color palette: with
/// `whole`, page 1's `/Resources` is object 6 and page 2's is object 9, an entry the
/// table puts at object 6; otherwise their `/ColorSpace` is object 7 and object 8, an
/// entry the table puts at object 7. Page 1's palette is red.
pub(crate) fn aliased(whole: bool) -> Vec<u8> {
    let page = |resources: &str| {
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 5 0 R \
             /Resources {resources} >>"
        )
    };
    let (first, second) = if whole {
        (page("6 0 R"), page("9 0 R"))
    } else {
        (
            page("<< /ColorSpace 7 0 R >>"),
            page("<< /ColorSpace 8 0 R >>"),
        )
    };
    file(
        &[
            Part::Object("<< /Type /Catalog /Pages 2 0 R >>", b""),
            Part::Object("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>", b""),
            Part::Object(&first, b""),
            Part::Object(&second, b""),
            Part::Stream(b"/CS cs 0 sc 0 0 50 50 re f"),
            Part::Object(RESOURCES, PALETTE),
            Part::Object(COLOR_SPACES, PALETTE),
        ],
        &[(8, 7), (9, 6)],
    )
}
