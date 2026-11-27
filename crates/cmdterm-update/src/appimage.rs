//! Embeds and reads the GPG signature of an AppImage.
//!
//! The type 2 AppImage runtime reserves two ELF sections for a signature:
//! `.sha256_sig` for an ASCII-armored detached signature and `.sig_key` for
//! the armored public key that made it. Both are zeros until signed. The
//! signed data is the hex SHA-256 digest of the whole file with both sections
//! zeroed, which is what `appimagetool --sign` signs and what AppImageUpdate's
//! `validate` and AppImageLauncher check. So a signature can be added to an
//! AppImage after it was built, without changing any other byte, and the
//! release workflow checks with [`same_appimage`] that the AppImage signed by
//! hand outside it is the one it built.

use sha2::{Digest, Sha256};
use std::ops::Range;

/// The section the armored signature goes into.
const SIGNATURE: &str = ".sha256_sig";
/// The section the armored public key goes into.
const KEY: &str = ".sig_key";

/// Where the two signature sections are in an AppImage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sections {
    pub signature: Range<usize>,
    pub key: Range<usize>,
}

fn read<const N: usize>(file: &[u8], at: u64) -> Result<[u8; N], String> {
    usize::try_from(at)
        .ok()
        .and_then(|at| file.get(at..at.checked_add(N)?))
        .map(|b| b.try_into().expect("N bytes"))
        .ok_or_else(|| "its ELF headers are cut off".to_owned())
}

fn u16_at(file: &[u8], at: u64) -> Result<u16, String> {
    read::<2>(file, at).map(u16::from_le_bytes)
}

fn u32_at(file: &[u8], at: u64) -> Result<u32, String> {
    read::<4>(file, at).map(u32::from_le_bytes)
}

fn u64_at(file: &[u8], at: u64) -> Result<u64, String> {
    read::<8>(file, at).map(u64::from_le_bytes)
}

/// Finds the signature sections of a type 2 AppImage: a little-endian 64-bit
/// ELF file with `AI\x02` at offset 8.
pub fn sections(file: &[u8]) -> Result<Sections, String> {
    if file.get(..4) != Some(b"\x7fELF") {
        return Err("it is not an ELF file".to_owned());
    }
    if file.get(8..11) != Some(b"AI\x02") {
        return Err("it is not a type 2 AppImage".to_owned());
    }
    if file[4] != 2 || file[5] != 1 {
        return Err("it is not a little-endian 64-bit ELF file".to_owned());
    }
    let table = u64_at(file, 0x28)?;
    let entry = u64::from(u16_at(file, 0x3a)?);
    let count = u64::from(u16_at(file, 0x3c)?);
    let names = u64::from(u16_at(file, 0x3e)?);
    if entry < 64 || names >= count {
        return Err("its section headers are malformed".to_owned());
    }
    let header = |index: u64| -> Result<(u32, u64, u64), String> {
        let at = index
            .checked_mul(entry)
            .and_then(|offset| offset.checked_add(table))
            .ok_or("its section headers are malformed")?;
        Ok((
            u32_at(file, at)?,
            u64_at(file, at + 24)?,
            u64_at(file, at + 32)?,
        ))
    };
    let range = |offset: u64, size: u64| -> Result<Range<usize>, String> {
        let start = usize::try_from(offset).map_err(|_| "a section ends past the file")?;
        let end = usize::try_from(size)
            .ok()
            .and_then(|size| start.checked_add(size))
            .filter(|&end| end <= file.len())
            .ok_or("a section ends past the file")?;
        Ok(start..end)
    };
    let (_, names_offset, names_size) = header(names)?;
    let names = &file[range(names_offset, names_size)?];
    let mut signature = None;
    let mut key = None;
    for index in 0..count {
        let (name, offset, size) = header(index)?;
        let name = names
            .get(name as usize..)
            .and_then(|rest| rest.split(|&b| b == 0).next())
            .unwrap_or_default();
        if name == SIGNATURE.as_bytes() {
            signature = Some(range(offset, size)?);
        } else if name == KEY.as_bytes() {
            key = Some(range(offset, size)?);
        }
    }
    let signature = signature.filter(|r| !r.is_empty());
    let key = key.filter(|r| !r.is_empty());
    match (signature, key) {
        (Some(signature), Some(key))
            if signature.end <= key.start || key.end <= signature.start =>
        {
            Ok(Sections { signature, key })
        }
        (Some(_), Some(_)) => Err("its signature sections overlap".to_owned()),
        _ => Err(format!("it has no {SIGNATURE} and {KEY} sections")),
    }
}

/// Whether `file` carries anything in its signature sections.
pub fn is_signed(file: &[u8], sections: &Sections) -> bool {
    file[sections.signature.clone()]
        .iter()
        .chain(&file[sections.key.clone()])
        .any(|&b| b != 0)
}

/// `file` with both signature sections zeroed.
fn unsigned_image(file: &[u8], sections: &Sections) -> Vec<u8> {
    let mut image = file.to_vec();
    image[sections.signature.clone()].fill(0);
    image[sections.key.clone()].fill(0);
    image
}

/// The data a signature covers: the lowercase hex SHA-256 of `file` with
/// both signature sections zeroed, without a newline.
pub fn digest(file: &[u8]) -> Result<String, String> {
    let sections = sections(file)?;
    let hash = Sha256::digest(unsigned_image(file, &sections));
    Ok(hash.iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes an armored `signature` and `key` into the sections of the
/// unsigned AppImage `file`.
pub fn embed(file: &mut [u8], signature: &[u8], key: &[u8]) -> Result<(), String> {
    let sections = sections(file)?;
    if is_signed(file, &sections) {
        return Err("it is signed already".to_owned());
    }
    for (what, data, range) in [
        ("signature", signature, &sections.signature),
        ("key", key, &sections.key),
    ] {
        if data.is_empty() || data.contains(&0) {
            return Err(format!("the {what} is empty or not armored"));
        }
        if data.len() > range.len() {
            return Err(format!(
                "the {what} has {} bytes, but its section only {}",
                data.len(),
                range.len()
            ));
        }
        file[range.start..range.start + data.len()].copy_from_slice(data);
    }
    Ok(())
}

/// The armored signature and key in a signed AppImage, without the zeros
/// after them.
pub fn signature(file: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let sections = sections(file)?;
    let text = |range: &Range<usize>| -> Vec<u8> {
        let section = &file[range.clone()];
        let end = section
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(section.len());
        section[..end].to_vec()
    };
    let (signature, key) = (text(&sections.signature), text(&sections.key));
    if signature.is_empty() || key.is_empty() {
        return Err("it is not signed".to_owned());
    }
    Ok((signature, key))
}

/// Checks that `signed` is the AppImage `unsigned` with a signature and a
/// key embedded and nothing else changed.
pub fn same_appimage(unsigned: &[u8], signed: &[u8]) -> Result<(), String> {
    let before = sections(unsigned).map_err(|e| format!("the unsigned file: {e}"))?;
    if is_signed(unsigned, &before) {
        return Err("the unsigned file: it is signed already".to_owned());
    }
    let after = sections(signed).map_err(|e| format!("the signed file: {e}"))?;
    signature(signed).map_err(|e| format!("the signed file: {e}"))?;
    if before != after || unsigned != unsigned_image(signed, &after).as_slice() {
        return Err("the signed file is a different AppImage".to_owned());
    }
    Ok(())
}
