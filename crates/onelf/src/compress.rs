//! Zstd compression utilities
//!
//! Provides streaming compression, dictionary support,
//! and block-based compression for large files.

use std::io;

pub const BLOCK_SIZE: u64 = 256 * 1024;

pub struct CompressedBlock {
    pub data: Vec<u8>,
    pub original_size: u64,
    /// BLAKE3 of the block's decompressed bytes, so a reader can verify
    /// this block alone rather than reassembling the whole entry.
    pub content_hash: [u8; 32],
}

pub fn compress(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    zstd::bulk::compress(data, level).map_err(io::Error::other)
}

pub fn compress_manifest(data: &[u8]) -> io::Result<Vec<u8>> {
    compress(data, 1)
}

pub fn build_dictionary(samples: &[Vec<u8>], dict_size: usize) -> io::Result<Vec<u8>> {
    let sizes: Vec<usize> = samples.iter().map(|s| s.len()).collect();
    let flat: Vec<u8> = samples.iter().flat_map(|s| s.iter().copied()).collect();
    zstd::dict::from_continuous(&flat, &sizes, dict_size).map_err(io::Error::other)
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
            content_hash: *blake3::hash(chunk).as_bytes(),
        });

        offset = chunk_end;
    }

    blocks
}

pub fn compress_in_blocks(
    data: &[u8],
    level: i32,
    dict: Option<&[u8]>,
) -> io::Result<Vec<CompressedBlock>> {
    let mut blocks = Vec::new();
    let mut offset = 0;

    // Build the dictionary-backed compressor once (digesting the dictionary
    // into its CDict a single time) and reuse it for every block, rather
    // than reconstructing it per block.
    let mut dict_compressor = match dict {
        Some(d) => Some(zstd::bulk::Compressor::with_dictionary(level, d)?),
        None => None,
    };

    while offset < data.len() {
        let chunk_end = (offset + BLOCK_SIZE as usize).min(data.len());
        let chunk = &data[offset..chunk_end];
        let original_size = chunk.len() as u64;

        let compressed = match dict_compressor.as_mut() {
            Some(c) => c.compress(chunk).map_err(io::Error::other)?,
            None => compress(chunk, level)?,
        };

        blocks.push(CompressedBlock {
            data: compressed,
            original_size,
            content_hash: *blake3::hash(chunk).as_bytes(),
        });

        offset = chunk_end;
    }

    Ok(blocks)
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
