//! Minimal ELF probing shared by the packer and the runtime.
//!
//! Only what both sides need to locate a program header, kept in one place so
//! a hardened parse cannot drift back into an unchecked duplicate.

/// Locate the PT_INTERP program header in `data` (which must contain the ELF
/// header and program-header table) and return the interpreter string's
/// `(file offset, size-in-file)`. Little-endian 32- and 64-bit ELF; every
/// read is bounds-checked against `data`. The interp bytes themselves may lie
/// beyond `data` (the caller reads them, possibly re-reading the file).
pub fn pt_interp_slot(data: &[u8]) -> Option<(usize, usize)> {
    if data.len() < 64 || data[0..4] != *b"\x7fELF" {
        return None;
    }
    let class = data[4];
    let (e_phoff, e_phentsize, e_phnum) = match class {
        2 => (
            u64::from_le_bytes(data.get(32..40)?.try_into().ok()?) as usize,
            u16::from_le_bytes(data.get(54..56)?.try_into().ok()?) as usize,
            u16::from_le_bytes(data.get(56..58)?.try_into().ok()?) as usize,
        ),
        1 => (
            u32::from_le_bytes(data.get(28..32)?.try_into().ok()?) as usize,
            u16::from_le_bytes(data.get(42..44)?.try_into().ok()?) as usize,
            u16::from_le_bytes(data.get(44..46)?.try_into().ok()?) as usize,
        ),
        _ => return None,
    };
    // Each program-header entry must be large enough to hold the fields we
    // read below (p_offset / p_filesz); reject malformed tables up front.
    let min_phentsize = if class == 2 { 56 } else { 32 };
    if e_phentsize < min_phentsize {
        return None;
    }
    for i in 0..e_phnum {
        let off = e_phoff.checked_add(i.checked_mul(e_phentsize)?)?;
        let end = off.checked_add(e_phentsize)?;
        if end > data.len() {
            break;
        }
        let p_type = u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?);
        if p_type != 3 {
            continue;
        }
        return match class {
            2 => Some((
                u64::from_le_bytes(data.get(off + 8..off + 16)?.try_into().ok()?) as usize,
                u64::from_le_bytes(data.get(off + 32..off + 40)?.try_into().ok()?) as usize,
            )),
            1 => Some((
                u32::from_le_bytes(data.get(off + 4..off + 8)?.try_into().ok()?) as usize,
                u32::from_le_bytes(data.get(off + 16..off + 20)?.try_into().ok()?) as usize,
            )),
            _ => None,
        };
    }
    None
}

/// The `PT_INTERP` string of the ELF file at `path`, or `None` for a file
/// that is not an ELF object or names no interpreter.
///
/// The first 8 KB cover the header and program-header table; the string
/// itself is read at its own offset, which `patchelf` can move far into
/// the file. Its length is bounded like a path, so a corrupt entry cannot
/// drive a large allocation.
pub fn read_interp(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const MAX_INTERP: usize = 4096;

    let mut file = std::fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut head = vec![0u8; 8192];
    let n = file.read(&mut head).ok()?;
    head.truncate(n);
    let (offset, size) = pt_interp_slot(&head)?;
    if size == 0 || size > MAX_INTERP {
        return None;
    }
    let end = offset.checked_add(size)?;
    if end as u64 > file_len {
        return None;
    }
    let bytes = match head.get(offset..end) {
        Some(s) => s.to_vec(),
        None => {
            file.seek(SeekFrom::Start(offset as u64)).ok()?;
            let mut buf = vec![0u8; size];
            file.read_exact(&mut buf).ok()?;
            buf
        }
    };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).ok().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-bit ELF whose single program header is a PT_INTERP pointing at
    /// `interp`, laid out immediately after the header table.
    fn elf64_with_interp(interp: &str) -> Vec<u8> {
        let phoff = 64usize;
        let phentsize = 56usize;
        let interp_off = phoff + phentsize;
        let mut v = vec![0u8; interp_off + interp.len() + 1];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // ELFCLASS64
        v[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        v[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes());
        v[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        v[phoff..phoff + 4].copy_from_slice(&3u32.to_le_bytes()); // PT_INTERP
        v[phoff + 8..phoff + 16].copy_from_slice(&(interp_off as u64).to_le_bytes());
        v[phoff + 32..phoff + 40].copy_from_slice(&((interp.len() + 1) as u64).to_le_bytes());
        v[interp_off..interp_off + interp.len()].copy_from_slice(interp.as_bytes());
        v
    }

    #[test]
    fn locates_the_interp_slot() {
        let name = "/lib64/ld-linux-x86-64.so.2";
        let elf = elf64_with_interp(name);
        let (off, sz) = pt_interp_slot(&elf).expect("slot");
        assert_eq!(off, 120);
        assert_eq!(sz, name.len() + 1);
        assert_eq!(&elf[off..off + sz - 1], name.as_bytes());
    }

    #[test]
    fn malformed_returns_none_without_panic() {
        assert!(pt_interp_slot(b"not an elf").is_none());
        assert!(pt_interp_slot(&[]).is_none());

        // ELF magic but a program-header table pointing off the end.
        let mut short = vec![0u8; 64];
        short[0..4].copy_from_slice(b"\x7fELF");
        short[4] = 2;
        short[56..58].copy_from_slice(&9999u16.to_le_bytes());
        short[32..40].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert!(pt_interp_slot(&short).is_none());
    }

    #[test]
    fn undersized_phentsize_is_rejected() {
        // A table whose entries are too small to hold the fields read below
        // would otherwise be walked with garbage offsets.
        let mut elf = elf64_with_interp("/lib/ld.so");
        elf[54..56].copy_from_slice(&8u16.to_le_bytes());
        assert!(pt_interp_slot(&elf).is_none());
    }
}
