//! Zstd compression utilities
//!
//! Provides streaming compression, dictionary support,
//! and block-based compression for large files.

use std::io::{self, Read};

/// Block size used when a package does not choose one. Blocks compress
/// independently, so this is both the window zstd gets and the unit the
/// runtime decompresses to serve a read. Starting an app reads scattered
/// pages of its libraries, so most of a large block is decompressed for
/// nothing: on a Blender startup 64 KiB launched 17 percent faster from a
/// cold cache than 256 KiB, for a package 5 percent larger, and 32 KiB was
/// slower again.
pub const DEFAULT_BLOCK_SIZE: u64 = 64 * 1024;

/// Smallest block a package may choose. Below this the per-block manifest
/// entry (56 bytes plus its hash) starts to rival the data it describes.
pub const MIN_BLOCK_SIZE: u64 = 4 * 1024;

/// Largest block a package may choose, matching the runtime's default FUSE
/// cache budget so a single block can always be held.
pub const MAX_BLOCK_SIZE: u64 = 32 * 1024 * 1024;

/// Block size of a stipplefs payload unless one is chosen. Its blocks
/// hold many files each, and the image is within a percent of its
/// smallest here at half the decode time of 64 MiB.
pub const DEFAULT_STIPPLE_BLOCK_SIZE: u64 = 32 * 1024 * 1024;

/// Largest block a stipplefs payload may choose. The runtime serves it in
/// 64 KiB slices, so its FUSE cache never has to hold one whole.
pub const MAX_STIPPLE_BLOCK_SIZE: u64 = 64 * 1024 * 1024;

/// One independently encoded and verified payload block.
#[derive(Debug)]
pub struct CompressedBlock {
    /// Compressed bytes, or unchanged bytes in store mode.
    pub data: Vec<u8>,
    /// Size of the original block before encoding.
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

/// Encoded blocks and the hash of their complete unencoded contents.
#[derive(Debug)]
pub struct EncodedFile {
    /// BLAKE3 of all input bytes, independent of block boundaries.
    pub content_hash: [u8; 32],
    /// Blocks in their input order, with no trailing empty block.
    pub blocks: Vec<CompressedBlock>,
}

/// Read and store independent blocks without retaining whole-file input.
pub fn store_in_blocks<R: Read>(reader: R, block_size: u64) -> io::Result<EncodedFile> {
    encode_in_blocks(reader, block_size, |block| Ok(block.to_vec()))
}

/// Read and compress independent blocks with one input buffer and compressor.
pub fn compress_in_blocks<R: Read>(
    reader: R,
    level: i32,
    dict: Option<&[u8]>,
    block_size: u64,
) -> io::Result<EncodedFile> {
    let mut compressor = match dict {
        Some(dict) => zstd::bulk::Compressor::with_dictionary(level, dict)?,
        None => zstd::bulk::Compressor::new(level)?,
    };
    encode_in_blocks(reader, block_size, |block| {
        compressor.compress(block).map_err(io::Error::other)
    })
}

fn encode_in_blocks<R: Read>(
    mut reader: R,
    block_size: u64,
    mut encode: impl FnMut(&[u8]) -> io::Result<Vec<u8>>,
) -> io::Result<EncodedFile> {
    if block_size == 0 || block_size > MAX_BLOCK_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "block size must be nonzero and no larger than MAX_BLOCK_SIZE",
        ));
    }
    let mut input = vec![0u8; block_size as usize];
    let mut hasher = blake3::Hasher::new();
    let mut blocks = Vec::new();
    loop {
        let mut filled = 0;
        while filled < input.len() {
            match reader.read(&mut input[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if filled == 0 {
            break;
        }
        let block = &input[..filled];
        hasher.update(block);
        blocks.push(CompressedBlock {
            data: encode(block)?,
            original_size: filled as u64,
            content_hash: *blake3::hash(block).as_bytes(),
        });
        if filled < input.len() {
            break;
        }
    }
    Ok(EncodedFile {
        content_hash: *hasher.finalize().as_bytes(),
        blocks,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_empty_yields_no_blocks() {
        let encoded = store_in_blocks(&[][..], DEFAULT_BLOCK_SIZE).unwrap();
        assert!(encoded.blocks.is_empty());
        assert_eq!(encoded.content_hash, *blake3::hash(&[]).as_bytes());
    }

    #[test]
    fn store_small_is_single_raw_block() {
        let data = b"hello world";
        let encoded = store_in_blocks(data.as_slice(), DEFAULT_BLOCK_SIZE).unwrap();
        assert_eq!(encoded.blocks.len(), 1);
        assert_eq!(encoded.blocks[0].data, data);
        assert_eq!(encoded.blocks[0].original_size, data.len() as u64);
        assert_eq!(encoded.content_hash, *blake3::hash(data).as_bytes());
    }

    #[test]
    fn store_chunks_on_block_size_and_roundtrips() {
        let bs = DEFAULT_BLOCK_SIZE as usize;
        let data: Vec<u8> = (0..bs * 2 + bs / 2).map(|i| (i % 251) as u8).collect();
        let encoded = store_in_blocks(data.as_slice(), DEFAULT_BLOCK_SIZE).unwrap();
        assert_eq!(encoded.blocks.len(), 3);
        assert_eq!(encoded.blocks[0].original_size, DEFAULT_BLOCK_SIZE);
        assert_eq!(encoded.blocks[1].original_size, DEFAULT_BLOCK_SIZE);
        assert_eq!(encoded.blocks[2].original_size, (bs / 2) as u64);
        assert_eq!(encoded.content_hash, *blake3::hash(&data).as_bytes());

        let mut joined = Vec::new();
        for block in &encoded.blocks {
            assert_eq!(block.data.len() as u64, block.original_size);
            assert_eq!(block.content_hash, *blake3::hash(&block.data).as_bytes());
            joined.extend_from_slice(&block.data);
        }
        assert_eq!(joined, data);
    }

    #[test]
    fn block_size_argument_drives_the_split() {
        let data: Vec<u8> = (0..(9 * 1024)).map(|i| (i % 251) as u8).collect();

        let stored = store_in_blocks(data.as_slice(), 4096).unwrap();
        assert_eq!(stored.blocks.len(), 3);
        assert_eq!(stored.blocks[0].original_size, 4096);
        assert_eq!(stored.blocks[2].original_size, 1024);

        let compressed = compress_in_blocks(data.as_slice(), 3, None, 4096).unwrap();
        assert_eq!(compressed.blocks.len(), 3);
        assert_eq!(compressed.blocks[0].original_size, 4096);
        assert_eq!(compressed.content_hash, stored.content_hash);

        let mut joined = Vec::new();
        for block in &compressed.blocks {
            joined.extend_from_slice(
                &zstd::bulk::decompress(&block.data, block.original_size as usize).unwrap(),
            );
        }
        assert_eq!(joined, data);
    }

    struct ShortReads<'a> {
        remaining: &'a [u8],
        interrupt: bool,
    }

    impl Read for ShortReads<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            assert!(output.len() <= 4096, "input reads must stay block-sized");
            if self.interrupt {
                self.interrupt = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = output.len().min(self.remaining.len()).min(17);
            output[..n].copy_from_slice(&self.remaining[..n]);
            self.remaining = &self.remaining[n..];
            Ok(n)
        }
    }

    #[test]
    fn short_and_interrupted_reads_preserve_block_bytes_and_hashes() {
        let data: Vec<u8> = (0..9000).map(|i| (i % 251) as u8).collect();
        for dict in [None, Some(b"shared dictionary contents".as_slice())] {
            let reader = ShortReads {
                remaining: &data,
                interrupt: true,
            };
            let encoded = compress_in_blocks(reader, 3, dict, 4096).unwrap();
            assert_eq!(encoded.content_hash, *blake3::hash(&data).as_bytes());
            assert_eq!(encoded.blocks.len(), 3);
            for (block, input) in encoded.blocks.iter().zip(data.chunks(4096)) {
                let expected = match dict {
                    Some(dict) => zstd::bulk::Compressor::with_dictionary(3, dict)
                        .unwrap()
                        .compress(input)
                        .unwrap(),
                    None => zstd::bulk::compress(input, 3).unwrap(),
                };
                assert_eq!(block.data, expected);
                assert_eq!(block.original_size, input.len() as u64);
                assert_eq!(block.content_hash, *blake3::hash(input).as_bytes());
            }
        }
        let stored = store_in_blocks(
            ShortReads {
                remaining: &data,
                interrupt: true,
            },
            4096,
        )
        .unwrap();
        assert_eq!(stored.content_hash, *blake3::hash(&data).as_bytes());
        for (block, input) in stored.blocks.iter().zip(data.chunks(4096)) {
            assert_eq!(block.data, input);
        }
    }

    #[test]
    fn exact_block_boundaries_have_no_empty_tail() {
        let data = vec![0x5a; 8192];
        let stored = store_in_blocks(data.as_slice(), 4096).unwrap();
        let compressed = compress_in_blocks(data.as_slice(), 3, None, 4096).unwrap();
        assert_eq!(stored.blocks.len(), 2);
        assert_eq!(compressed.blocks.len(), 2);
        let empty = compress_in_blocks(&[][..], 3, None, 4096).unwrap();
        assert!(empty.blocks.is_empty());
        assert_eq!(empty.content_hash, *blake3::hash(&[]).as_bytes());
    }

    #[test]
    fn read_errors_are_not_treated_as_eof() {
        let input = &b"partial"[..];
        let failure = io::Error::new(io::ErrorKind::PermissionDenied, "unreadable input");
        let reader = input.chain(FailingReader(failure));
        let error = compress_in_blocks(reader, 3, None, 4096).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    struct FailingReader(io::Error);

    impl Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0.kind(), self.0.to_string()))
        }
    }

    #[test]
    fn invalid_block_sizes_fail_before_reading() {
        for size in [0, MAX_BLOCK_SIZE + 1, u64::MAX] {
            let reader = FailingReader(io::ErrorKind::PermissionDenied.into());
            let error = store_in_blocks(reader, size).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }
}
