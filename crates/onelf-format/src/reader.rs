//! Bounds checking for the regions a package footer describes.
//!
//! A packed file is untrusted input: the runtime parses one on an end user's
//! machine, and the packer's inspection commands parse whatever they are
//! handed. Every offset and size in the footer, and in the blocks the
//! manifest carries, is an attacker-controlled `u64`.
//!
//! These checks used to live only in the runtime, so `onelf info` on a
//! truncated download tried to allocate whatever the footer claimed and
//! aborted. Both sides now validate here, before allocating or seeking.

use std::io;

use crate::entry::Block;
use crate::footer::Footer;

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// True when `off + len` stays within `file_size` without overflowing.
fn in_bounds(off: u64, len: u64, file_size: u64) -> bool {
    off.checked_add(len).is_some_and(|end| end <= file_size)
}

/// Check every region `footer` points at against the real file size.
///
/// Callers MUST run this before acting on any footer field. It is what makes
/// a later `vec![0; size]` safe to perform.
pub fn validate_footer(footer: &Footer, file_size: u64) -> io::Result<()> {
    if !in_bounds(
        footer.manifest_offset,
        footer.manifest_compressed,
        file_size,
    ) {
        return Err(invalid("manifest region out of bounds"));
    }
    if !in_bounds(footer.payload_offset, footer.payload_size, file_size) {
        return Err(invalid("payload region out of bounds"));
    }
    // The manifest decompresses into a buffer sized by this field, so it has
    // to be backed by something even though it describes no file region.
    if footer.manifest_original > file_size.saturating_mul(MAX_MANIFEST_EXPANSION) {
        return Err(invalid("manifest expands implausibly"));
    }
    if footer.dict_size > 0 && !in_bounds(footer.dict_offset, footer.dict_size as u64, file_size) {
        return Err(invalid("dictionary region out of bounds"));
    }
    Ok(())
}

/// Ceiling on how far the manifest may claim to decompress, relative to the
/// whole file. Compressed manifests are small and text-like; anything beyond
/// this is a crafted header rather than a real package.
const MAX_MANIFEST_EXPANSION: u64 = 1024;

/// Absolute file offset of `block`'s compressed bytes, checked against the
/// payload region `footer` declares.
///
/// Returns the offset and the compressed length, so the caller can allocate
/// knowing the file can back it.
pub fn block_extent(footer: &Footer, block: &Block) -> io::Result<(u64, usize)> {
    let abs = footer
        .payload_offset
        .checked_add(block.payload_offset)
        .ok_or_else(|| invalid("payload offset overflow"))?;
    let payload_end = footer
        .payload_offset
        .checked_add(footer.payload_size)
        .ok_or_else(|| invalid("payload region overflow"))?;
    if !in_bounds(abs, block.compressed_size, payload_end) {
        return Err(invalid("block extends past the payload region"));
    }
    let len = usize::try_from(block.compressed_size)
        .map_err(|_| invalid("block larger than this address space"))?;
    Ok((abs, len))
}

/// Decompressed size of `block`, rejected when it claims to expand further
/// than any real content would.
///
/// Both zstd and the dictionary path size their output buffer from this, so
/// an unchecked value is an allocation an attacker chooses.
pub fn block_original_size(block: &Block) -> io::Result<usize> {
    if block.compressed_size > 0
        && block.original_size / block.compressed_size > MAX_BLOCK_EXPANSION
    {
        return Err(invalid("block expands implausibly"));
    }
    usize::try_from(block.original_size)
        .map_err(|_| invalid("block larger than this address space"))
}

/// Ceiling on a single block's compression ratio. zstd tops out far below
/// this even on pathological input, so exceeding it means a crafted header.
const MAX_BLOCK_EXPANSION: u64 = 4096;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footer::Flags;

    fn footer() -> Footer {
        Footer {
            format_version: 1,
            flags: Flags::empty(),
            manifest_offset: 100,
            manifest_compressed: 50,
            manifest_original: 200,
            payload_offset: 200,
            payload_size: 300,
            dict_offset: 0,
            dict_size: 0,
            manifest_checksum: [0; 4],
        }
    }

    fn block(offset: u64, compressed: u64, original: u64) -> Block {
        Block {
            payload_offset: offset,
            compressed_size: compressed,
            original_size: original,
        }
    }

    #[test]
    fn well_formed_footer_passes() {
        assert!(validate_footer(&footer(), 1000).is_ok());
    }

    #[test]
    fn regions_past_the_file_are_rejected() {
        let mut f = footer();
        f.manifest_compressed = u64::MAX;
        assert!(validate_footer(&f, 1000).is_err());

        let mut f = footer();
        f.payload_size = 10_000;
        assert!(validate_footer(&f, 1000).is_err());

        let mut f = footer();
        f.dict_size = 900;
        f.dict_offset = 500;
        assert!(validate_footer(&f, 1000).is_err());
    }

    #[test]
    fn overflowing_offsets_are_rejected() {
        let mut f = footer();
        f.manifest_offset = u64::MAX;
        f.manifest_compressed = 1;
        assert!(validate_footer(&f, 1000).is_err());
    }

    #[test]
    fn absurd_manifest_expansion_is_rejected() {
        let mut f = footer();
        f.manifest_original = u64::MAX;
        assert!(validate_footer(&f, 1000).is_err());
    }

    #[test]
    fn block_within_payload_resolves() {
        let f = footer();
        let (abs, len) = block_extent(&f, &block(10, 20, 40)).unwrap();
        assert_eq!((abs, len), (210, 20));
    }

    #[test]
    fn block_past_the_payload_is_rejected() {
        let f = footer();
        assert!(block_extent(&f, &block(290, 20, 40)).is_err());
        assert!(block_extent(&f, &block(u64::MAX, 1, 1)).is_err());
        assert!(block_extent(&f, &block(0, u64::MAX, 1)).is_err());
    }

    #[test]
    fn absurd_block_expansion_is_rejected() {
        assert!(block_original_size(&block(0, 10, 40)).is_ok());
        assert!(block_original_size(&block(0, 1, u64::MAX)).is_err());
        // A stored block reports equal sizes and must stay acceptable.
        assert!(block_original_size(&block(0, 4096, 4096)).is_ok());
    }
}
