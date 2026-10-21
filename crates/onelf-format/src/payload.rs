//! Turning a package's payload back into file content.
//!
//! A payload is either a run of separately compressed blocks or a stipplefs image.
//! A stipplefs payload keeps the manifest exactly as it is otherwise, with
//! every file and its blocks, but a block is then a slice of its file:
//! `payload_offset` is where the slice starts within the file,
//! `original_size` is its length, and `compressed_size` is the file's inode
//! in the image. The BLAKE3 a block carries covers the slice, so a reader
//! verifies what the image hands it the same way it verifies a decompressed
//! onelf block, and the manifest stays the only thing a package's integrity
//! rests on.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use stipplefs::read::{Image, Reader};
use stipplefs::source::ByteSource;

use crate::{Block, EntryKind, Flags, Footer, Manifest};

/// Size of the slices a stipplefs payload's files are verified and cached
/// in, whatever block size the image itself was built with.
pub const SLICE_SIZE: u64 = 64 * 1024;

/// How a package's blocks become file content.
#[derive(Clone)]
pub enum Codec {
    /// A blocks payload: zstd frames, compressed with the package's
    /// dictionary when it has one, or raw bytes when the footer says the
    /// payload is stored.
    Zstd(Option<Arc<[u8]>>),
    /// A stipplefs image, opened once for the life of the process.
    Stipple(&'static Reader<'static>),
}

impl Codec {
    /// The codec for the payload `footer` describes. The footer must
    /// already have passed [`crate::reader::validate_footer`].
    pub fn open(file: &File, footer: &Footer) -> io::Result<Codec> {
        if footer.is_stipple() {
            let source = FileSource {
                size: file.metadata()?.len(),
                file: file.try_clone()?,
            };
            let image = Image::new(source, footer.payload_offset, footer.payload_size)
                .map_err(stipple_error)?;
            let image: &'static Image<'static> = Box::leak(Box::new(image));
            let reader = image.reader().map_err(stipple_error)?;
            return Ok(Codec::Stipple(Box::leak(Box::new(reader))));
        }
        if !footer.flags.contains(Flags::HAS_DICT) || footer.dict_size == 0 {
            return Ok(Codec::Zstd(None));
        }
        let mut dict = vec![0u8; footer.dict_size as usize];
        file.read_exact_at(&mut dict, footer.dict_offset)?;
        Ok(Codec::Zstd(Some(dict.into())))
    }

    /// The dictionary a blocks payload was compressed with, if any.
    pub fn dict(&self) -> Option<&[u8]> {
        match self {
            Codec::Zstd(dict) => dict.as_deref(),
            Codec::Stipple(_) => None,
        }
    }
}

/// The slice of a file `block` describes, read out of a stipplefs image.
/// Unverified: the caller checks it against the block's hash.
pub fn read_slice(reader: &Reader<'_>, block: &Block) -> io::Result<Vec<u8>> {
    let ino = u32::try_from(block.compressed_size)
        .map_err(|_| invalid("a block names an inode outside the image"))?;
    let mut buf = vec![0u8; crate::reader::block_original_size(block)?];
    let read = reader
        .read(ino, block.payload_offset, &mut buf)
        .map_err(stipple_error)?;
    if read != buf.len() {
        return Err(invalid(
            "a file in the image is shorter than the manifest says",
        ));
    }
    Ok(buf)
}

/// The file entries of `manifest`, in the order to read many of them in.
///
/// That is the order their content sits in the payload. A stipplefs image
/// places files by type and by start-up order rather than by path, and
/// reading them by path would decode a large block again for every file
/// it holds.
pub fn file_order(manifest: &Manifest, codec: &Codec) -> Vec<usize> {
    let mut files: Vec<usize> = (0..manifest.entries.len())
        .filter(|&i| manifest.entries[i].kind == EntryKind::File)
        .collect();
    if let Codec::Stipple(reader) = codec {
        files.sort_by_cached_key(|&i| {
            manifest.entries[i]
                .blocks
                .first()
                .and_then(|b| u32::try_from(b.compressed_size).ok())
                .and_then(|ino| reader.file_extents(ino).first().map(|e| e.0))
                .unwrap_or(u64::MAX)
        });
    }
    files
}

/// The package file, as the byte source an image is read from.
struct FileSource {
    file: File,
    size: u64,
}

impl ByteSource for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> stipplefs::error::Result<()> {
        Ok(self.file.read_exact_at(buf, offset)?)
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn stipple_error(e: stipplefs::error::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("stipplefs: {e}"))
}
