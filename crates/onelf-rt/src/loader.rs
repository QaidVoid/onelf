//! Package loading from the current binary.
//!
//! Reads the ONELF footer from the end of `/proc/self/exe`, decompresses the
//! manifest, and optionally loads the zstd dictionary.

use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};

use onelf_format::{Entry, FOOTER_SIZE, Footer, Manifest};

pub struct PackageData {
    pub footer: Footer,
    pub manifest: Manifest,
    pub file: File,
    pub dict: Option<Vec<u8>>,
}

pub fn load() -> io::Result<PackageData> {
    let mut file = File::open("/proc/self/exe")?;
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

    // Validate every region the footer points at against the real file
    // size before trusting any offset/size taken from it. Guards both
    // out-of-bounds reads and overflow in offset+size arithmetic.
    let in_bounds = |off: u64, len: u64| off.checked_add(len).is_some_and(|e| e <= file_size);
    if !in_bounds(footer.manifest_offset, footer.manifest_compressed) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest region out of bounds",
        ));
    }
    if !in_bounds(footer.payload_offset, footer.payload_size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload region out of bounds",
        ));
    }

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
    // (matches what the packer writes) before trusting the bytes.
    if xxhash_rust::xxh32::xxh32(&manifest_bytes, 0).to_le_bytes() != footer.manifest_checksum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest checksum mismatch",
        ));
    }

    let manifest = Manifest::deserialize(&manifest_bytes)?;

    // Read dictionary if present
    let dict = if footer.flags.contains(onelf_format::Flags::HAS_DICT) && footer.dict_size > 0 {
        if !in_bounds(footer.dict_offset, footer.dict_size as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "dictionary region out of bounds",
            ));
        }
        file.seek(SeekFrom::Start(footer.dict_offset))?;
        let mut dict_buf = vec![0u8; footer.dict_size as usize];
        file.read_exact(&mut dict_buf)?;
        Some(dict_buf)
    } else {
        None
    };

    Ok(PackageData {
        footer,
        manifest,
        file,
        dict,
    })
}

pub fn read_payload_entry(
    file: &mut File,
    payload_offset: u64,
    entry_offset: u64,
    compressed_size: u64,
    original_size: u64,
    dict: Option<&[u8]>,
    stored: bool,
) -> io::Result<Vec<u8>> {
    let abs = payload_offset
        .checked_add(entry_offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "payload offset overflow"))?;
    file.seek(SeekFrom::Start(abs))?;
    let mut buf = vec![0u8; compressed_size as usize];
    file.read_exact(&mut buf)?;

    // Store mode: bytes are the file content verbatim, no zstd.
    if stored {
        return Ok(buf);
    }

    let data = if let Some(d) = dict {
        let cursor = Cursor::new(&buf);
        let mut decoder = zstd::Decoder::with_dictionary(cursor, d)?;
        let mut result = Vec::with_capacity(original_size as usize);
        decoder.read_to_end(&mut result)?;
        result
    } else {
        zstd::bulk::decompress(&buf, original_size as usize).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("decompression: {e}"))
        })?
    };

    Ok(data)
}

/// Read and reassemble an entry's payload, then verify it against the
/// entry's recorded BLAKE3 `content_hash` before returning. A mismatch
/// (tampered or corrupt package, or a poisoned content-addressable store
/// slot) is a hard error, so unverified bytes never reach execution,
/// hardlinking, memfd loading, or FUSE.
pub fn read_verified_entry(
    file: &mut File,
    footer: &Footer,
    entry: &Entry,
    dict: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    let data = read_payload_blocks(
        file,
        footer.payload_offset,
        &entry.blocks,
        dict,
        footer.is_stored(),
    )?;
    if blake3::hash(&data).as_bytes() != &entry.content_hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "onelf: content hash mismatch (tampered or corrupt package)",
        ));
    }
    Ok(data)
}

pub fn read_payload_blocks(
    file: &mut File,
    payload_offset: u64,
    blocks: &[onelf_format::Block],
    dict: Option<&[u8]>,
    stored: bool,
) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();

    for block in blocks {
        let abs = payload_offset
            .checked_add(block.payload_offset)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "payload offset overflow"))?;
        file.seek(SeekFrom::Start(abs))?;
        let mut buf = vec![0u8; block.compressed_size as usize];
        file.read_exact(&mut buf)?;

        // Store mode: bytes are the file content verbatim, no zstd.
        if stored {
            result.extend_from_slice(&buf);
            continue;
        }

        let decompressed = if let Some(d) = dict {
            let cursor = Cursor::new(&buf);
            let mut decoder = zstd::Decoder::with_dictionary(cursor, d)?;
            let mut block_result = Vec::with_capacity(block.original_size as usize);
            decoder.read_to_end(&mut block_result)?;
            block_result
        } else {
            zstd::bulk::decompress(&buf, block.original_size as usize).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("decompression: {e}"))
            })?
        };

        result.extend_from_slice(&decompressed);
    }

    Ok(result)
}
