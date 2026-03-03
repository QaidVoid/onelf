//! Zstd compression utilities
//!
//! Provides streaming compression, dictionary support,
//! and block-based compression for large files.

use std::io;

pub const BLOCK_SIZE: u64 = 256 * 1024;

pub struct CompressedBlock {
    pub data: Vec<u8>,
    pub original_size: u64,
}

pub fn compress(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    zstd::bulk::compress(data, level).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

pub fn compress_manifest(data: &[u8]) -> io::Result<Vec<u8>> {
    compress(data, 1)
}

pub fn build_dictionary(samples: &[Vec<u8>], dict_size: usize) -> io::Result<Vec<u8>> {
    let sizes: Vec<usize> = samples.iter().map(|s| s.len()).collect();
    let flat: Vec<u8> = samples.iter().flat_map(|s| s.iter().copied()).collect();
    zstd::dict::from_continuous(&flat, &sizes, dict_size)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

pub fn compress_with_dict(data: &[u8], level: i32, dict: &[u8]) -> io::Result<Vec<u8>> {
    let mut compressor = zstd::bulk::Compressor::with_dictionary(level, dict)?;
    compressor
        .compress(data)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

pub fn compress_in_blocks(
    data: &[u8],
    level: i32,
    dict: Option<&[u8]>,
) -> io::Result<Vec<CompressedBlock>> {
    let mut blocks = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        let chunk_end = (offset + BLOCK_SIZE as usize).min(data.len());
        let chunk = &data[offset..chunk_end];
        let original_size = chunk.len() as u64;

        let compressed = if let Some(d) = dict {
            compress_with_dict(chunk, level, d)?
        } else {
            compress(chunk, level)?
        };

        blocks.push(CompressedBlock {
            data: compressed,
            original_size,
        });

        offset = chunk_end;
    }

    Ok(blocks)
}
