//! Package loading from the current binary, or from any package file.
//!
//! Reads the ONELF footer from the end of the file, decompresses the
//! manifest, and opens the payload's codec.

use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;

use onelf_format::payload::Codec;
use onelf_format::{Entry, FOOTER_SIZE, Footer, Manifest};

pub struct PackageData {
    pub footer: Footer,
    pub manifest: Manifest,
    pub file: File,
    pub codec: Codec,
}

/// The package this process is running from.
pub fn load() -> io::Result<PackageData> {
    load_from(Path::new("/proc/self/exe"))
}

/// The package at `path`, which is how a pinned GL build is opened.
pub fn load_from(path: &Path) -> io::Result<PackageData> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();

    if file_size < FOOTER_SIZE as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "binary too small",
        ));
    }

    // Read footer from the last FOOTER_SIZE bytes
    file.seek(SeekFrom::End(-(FOOTER_SIZE as i64)))?;
    let mut footer_buf = [0u8; FOOTER_SIZE];
    file.read_exact(&mut footer_buf)?;
    let footer = Footer::from_bytes(&footer_buf)?;
    onelf_format::reader::validate_footer(&footer, file_size)?;

    // Read and decompress manifest
    file.seek(SeekFrom::Start(footer.manifest_offset))?;
    let mut manifest_compressed = vec![0u8; footer.manifest_compressed as usize];
    file.read_exact(&mut manifest_compressed)?;

    let manifest_bytes =
        zstd::bulk::decompress(&manifest_compressed, footer.manifest_original as usize).map_err(
            |e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("manifest decompression: {e}"),
                )
            },
        )?;

    // Verify the footer's XXH32 checksum over the uncompressed manifest
    // (with the footer_hash header field excluded; matches what the
    // packer writes) before trusting the bytes.
    if onelf_format::manifest::manifest_checksum(&manifest_bytes)? != footer.manifest_checksum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest checksum mismatch",
        ));
    }

    let manifest = Manifest::deserialize(&manifest_bytes)?;
    let codec = Codec::open(&file, &footer)?;

    Ok(PackageData {
        footer,
        manifest,
        file,
        codec,
    })
}

/// Read and decompress a single payload block, verifying it against its
/// recorded hash before returning.
///
/// This is what lets the FUSE server serve a slice of a large file without
/// reassembling all of it: the check is per block, so memory stays
/// proportional to the read rather than to the entry. Blocks from a
/// version-1 manifest carry no hash, and the caller falls back to the
/// whole-entry check for those. The read leaves the file position alone, so
/// the FUSE server's prefetch workers can share the file.
pub fn read_payload_entry(
    file: &File,
    footer: &Footer,
    block: &onelf_format::Block,
    codec: &Codec,
) -> io::Result<Vec<u8>> {
    let data = read_block(file, footer, block, codec)?;
    if block.has_content_hash() && blake3::hash(&data).as_bytes() != &block.content_hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "onelf: block hash mismatch (tampered or corrupt package)",
        ));
    }
    Ok(data)
}

/// Read and reassemble an entry's payload, then verify it against the
/// entry's recorded BLAKE3 `content_hash` before returning. A mismatch
/// (tampered or corrupt package, or a poisoned content-addressable store
/// slot) is a hard error, so unverified bytes never reach execution,
/// hardlinking, memfd loading, or FUSE.
pub fn read_verified_entry(
    file: &File,
    footer: &Footer,
    entry: &Entry,
    codec: &Codec,
) -> io::Result<Vec<u8>> {
    let data = read_payload_blocks(file, footer, &entry.blocks, codec)?;
    if blake3::hash(&data).as_bytes() != &entry.content_hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "onelf: content hash mismatch (tampered or corrupt package)",
        ));
    }
    Ok(data)
}

/// Read and concatenate every block of an entry.
pub fn read_payload_blocks(
    file: &File,
    footer: &Footer,
    blocks: &[onelf_format::Block],
    codec: &Codec,
) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    for block in blocks {
        result.extend_from_slice(&read_block(file, footer, block, codec)?);
    }
    Ok(result)
}

/// One block's content, unverified. The read leaves the file position
/// alone, so the FUSE server's prefetch workers can share the file.
fn read_block(
    file: &File,
    footer: &Footer,
    block: &onelf_format::Block,
    codec: &Codec,
) -> io::Result<Vec<u8>> {
    if let Codec::Stipple(reader) = codec {
        return onelf_format::payload::read_slice(reader, block);
    }
    let (abs, len) = onelf_format::reader::block_extent(footer, block)?;
    let mut buf = vec![0u8; len];
    file.read_exact_at(&mut buf, abs)?;
    // Store mode: bytes are the file content verbatim, no zstd.
    if footer.is_stored() {
        return Ok(buf);
    }
    let original = onelf_format::reader::block_original_size(block)?;
    decode_block(&buf, original, codec.dict())
}

/// Decompress one block that the manifest says is `original` bytes long.
///
/// Both paths are held to that length. The dictionary decoder streams, so
/// without a bound a few kilobytes of input could expand without limit
/// before the hash check; and a block that comes up short is an error,
/// since on a v1 manifest with no per-block hash it would be served with
/// every later offset shifted.
fn decode_block(buf: &[u8], original: usize, dict: Option<&[u8]>) -> io::Result<Vec<u8>> {
    let invalid = |msg: String| io::Error::new(io::ErrorKind::InvalidData, msg);
    let data = match dict {
        Some(d) => {
            let decoder = zstd::Decoder::with_dictionary(Cursor::new(buf), d)?;
            let mut out = Vec::with_capacity(original);
            decoder.take(original as u64 + 1).read_to_end(&mut out)?;
            out
        }
        None => zstd::bulk::decompress(buf, original)
            .map_err(|e| invalid(format!("decompression: {e}")))?,
    };
    if data.len() != original {
        return Err(invalid(format!(
            "block decompressed to {} bytes, manifest says {original}",
            data.len()
        )));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DICT: &[u8] = b"a raw content dictionary shared by every block in this test";

    fn compressed(data: &[u8], dict: Option<&[u8]>) -> Vec<u8> {
        match dict {
            Some(d) => zstd::bulk::Compressor::with_dictionary(3, d)
                .unwrap()
                .compress(data)
                .unwrap(),
            None => zstd::bulk::compress(data, 3).unwrap(),
        }
    }

    #[test]
    fn a_block_must_decode_to_exactly_the_length_the_manifest_claims() {
        let data = b"hello hello hello hello hello world".repeat(50);
        for dict in [None, Some(DICT)] {
            let buf = compressed(&data, dict);
            assert_eq!(decode_block(&buf, data.len(), dict).unwrap(), data);
            assert!(decode_block(&buf, data.len() - 1, dict).is_err());
            assert!(decode_block(&buf, data.len() + 1, dict).is_err());
        }
    }
}
