//! Package loading from the current binary.
//!
//! Reads the ONELF footer from the end of `/proc/self/exe`, decompresses the
//! manifest, and optionally loads the zstd dictionary.

use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};

use onelf_format::{FOOTER_SIZE, Footer, Manifest};

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

    let manifest = Manifest::deserialize(&manifest_bytes)?;

    // Read dictionary if present
    let dict = if footer.flags.contains(onelf_format::Flags::HAS_DICT) && footer.dict_size > 0 {
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
) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(payload_offset + entry_offset))?;
    let mut compressed = vec![0u8; compressed_size as usize];
    file.read_exact(&mut compressed)?;

    let data = if let Some(d) = dict {
        let cursor = Cursor::new(&compressed);
        let mut decoder = zstd::Decoder::with_dictionary(cursor, d)?;
        let mut result = Vec::with_capacity(original_size as usize);
        decoder.read_to_end(&mut result)?;
        result
    } else {
        zstd::bulk::decompress(&compressed, original_size as usize).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("decompression: {e}"))
        })?
    };

    Ok(data)
}

pub fn read_payload_blocks(
    file: &mut File,
    payload_offset: u64,
    blocks: &[onelf_format::Block],
    dict: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();

    for block in blocks {
        file.seek(SeekFrom::Start(payload_offset + block.payload_offset))?;
        let mut compressed = vec![0u8; block.compressed_size as usize];
        file.read_exact(&mut compressed)?;

        let decompressed = if let Some(d) = dict {
            let cursor = Cursor::new(&compressed);
            let mut decoder = zstd::Decoder::with_dictionary(cursor, d)?;
            let mut block_result = Vec::with_capacity(block.original_size as usize);
            decoder.read_to_end(&mut block_result)?;
            block_result
        } else {
            zstd::bulk::decompress(&compressed, block.original_size as usize).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("decompression: {e}"))
            })?
        };

        result.extend_from_slice(&decompressed);
    }

    Ok(result)
}
