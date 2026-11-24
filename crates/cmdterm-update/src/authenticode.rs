//! Compares a Windows program with its Authenticode-signed copy.
//!
//! Signing a PE file changes three things: the checksum in the optional
//! header, the certificate table entry among the data directories, and the
//! end of the file, where the signature goes after up to 7 bytes of padding.
//! [`image`] takes all three out, so a program and its signed copy have the
//! same image exactly when signing changed nothing else. The release workflow
//! uses this to check that the programs signed by hand outside it are the
//! ones it built.

/// The checksum's offset in the optional header, for PE32 and PE32+.
const CHECKSUM: usize = 64;
/// The certificate table's index among the data directories.
const CERTIFICATE_TABLE: usize = 4;

/// Where the fields signing changes are in a PE file.
struct Layout {
    checksum: usize,
    certificate_table: usize,
}

fn read_u16(file: &[u8], at: usize) -> Result<u16, String> {
    file.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| "its headers are cut off".to_owned())
}

fn read_u32(file: &[u8], at: usize) -> Result<u32, String> {
    file.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| "its headers are cut off".to_owned())
}

fn layout(file: &[u8]) -> Result<Layout, String> {
    if file.get(..2) != Some(b"MZ") {
        return Err("it is not a Windows program".to_owned());
    }
    let pe = read_u32(file, 0x3c)? as usize;
    if file.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("it has no PE header".to_owned());
    }
    let optional = pe + 24;
    let (count, directories) = match read_u16(file, optional)? {
        0x10b => (optional + 92, optional + 96),
        0x20b => (optional + 108, optional + 112),
        magic => {
            return Err(format!(
                "its optional header has the unknown magic {magic:#x}"
            ));
        }
    };
    if read_u32(file, count)? as usize <= CERTIFICATE_TABLE {
        return Err("it has no certificate table entry".to_owned());
    }
    let certificate_table = directories + CERTIFICATE_TABLE * 8;
    read_u32(file, certificate_table + 4)?;
    Ok(Layout {
        checksum: optional + CHECKSUM,
        certificate_table,
    })
}

/// The bytes of `file` that signing does not change: everything but its
/// checksum, its certificate table entry, the certificate table and the zeros
/// before and after it. `signed` says whether `file` must carry a signature
/// or must not.
pub fn image(file: &[u8], signed: bool) -> Result<Vec<u8>, String> {
    let layout = layout(file)?;
    let offset = read_u32(file, layout.certificate_table)? as usize;
    let size = read_u32(file, layout.certificate_table + 4)? as usize;
    let end = match (signed, size) {
        (false, 0) => file.len(),
        (false, _) => return Err("it is signed already".to_owned()),
        (true, 0) => return Err("it is not signed".to_owned()),
        (true, _) => {
            let table_end = offset
                .checked_add(size)
                .filter(|&end| end <= file.len())
                .ok_or("its certificate table ends past the file")?;
            if offset < layout.certificate_table + 8 {
                return Err("its certificate table overlaps the headers".to_owned());
            }
            // Nothing the signature does not cover may follow it.
            if file[table_end..].iter().any(|&b| b != 0) {
                return Err("it has data after its signature".to_owned());
            }
            offset
        }
    };
    let mut image = file[..end].to_vec();
    image[layout.checksum..layout.checksum + 4].fill(0);
    image[layout.certificate_table..layout.certificate_table + 8].fill(0);
    let kept = image
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |last| last + 1);
    image.truncate(kept);
    Ok(image)
}

/// Checks that `signed` is `unsigned` with an Authenticode signature added
/// and nothing else changed.
pub fn same_program(unsigned: &[u8], signed: &[u8]) -> Result<(), String> {
    let before = image(unsigned, false).map_err(|e| format!("the unsigned file: {e}"))?;
    let after = image(signed, true).map_err(|e| format!("the signed file: {e}"))?;
    if before != after {
        return Err("the signed file is a different program".to_owned());
    }
    Ok(())
}
