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

/// Chunk `data` into `BLOCK_SIZE` pieces without compressing. Used by
/// store mode (`--no-compress`): the runtime reads these bytes directly,
/// so `data.len() == original_size` for every block. Produces the same
/// block layout as `compress_in_blocks` (empty input -> no blocks).
pub fn store_in_blocks(data: &[u8]) -> Vec<CompressedBlock> {
    let mut blocks = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        let chunk_end = (offset + BLOCK_SIZE as usize).min(data.len());
        let chunk = &data[offset..chunk_end];

        blocks.push(CompressedBlock {
            data: chunk.to_vec(),
            original_size: chunk.len() as u64,
        });

        offset = chunk_end;
    }

    blocks
}

#[cfg(test)]
mod store_tests {
    use super::*;

    #[test]
    fn store_empty_yields_no_blocks() {
        assert!(store_in_blocks(&[]).is_empty());
    }

    #[test]
    fn store_small_is_single_raw_block() {
        let data = b"hello world";
        let blocks = store_in_blocks(data);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].data, data);
        assert_eq!(blocks[0].original_size, data.len() as u64);
        // Store mode invariant: compressed_size == original_size.
        assert_eq!(blocks[0].data.len() as u64, blocks[0].original_size);
    }

    #[test]
    fn store_chunks_on_block_size_and_roundtrips() {
        let bs = BLOCK_SIZE as usize;
        let data: Vec<u8> = (0..bs * 2 + bs / 2).map(|i| (i % 251) as u8).collect();
        let blocks = store_in_blocks(&data);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].original_size, BLOCK_SIZE);
        assert_eq!(blocks[1].original_size, BLOCK_SIZE);
        assert_eq!(blocks[2].original_size, (bs / 2) as u64);

        let mut joined = Vec::new();
        for b in &blocks {
            assert_eq!(b.data.len() as u64, b.original_size);
            joined.extend_from_slice(&b.data);
        }
        assert_eq!(joined, data);
    }
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
