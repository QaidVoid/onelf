//! Footer structure and serialization
//!
//! The footer is located at the end of every ONELF package and contains:
//! - Magic bytes for identification
//! - Format version
//! - Offsets to manifest, payload, and optional dictionary
//! - Checksums for integrity verification
//!
//! # Structure
//!
//! The footer is exactly 76 bytes and is organized as follows:
//!
//! ```text
//! Offset  Size    Field
//! ------  -------  -------------------
//! 0      8        Magic: "ONELF\0\x01\x00"
//! 8      2        Format version (u16)
//! 10     2        Flags (u16)
//! 12     8        Manifest offset (u64)
//! 20     8        Manifest compressed size (u64)
//! 28     8        Manifest original size (u64)
//! 36     8        Payload offset (u64)
//! 44     8        Payload total size (u64)
//! 52     8        Dictionary offset (u64)
//! 60     4        Dictionary size (u32)
//! 64     4        Manifest checksum (xxh32)
//! 68     8        End magic: "FLENONE\x00"
//!//!
//! # Example
//!
//! no_run
//! use onelf_format::Footer;
//!
//! let footer = Footer {
//!     format_version: 1,
//!     // ... other fields
//! };
//!

use std::io::{self, Read, Write};

pub const FOOTER_SIZE: usize = 76;
pub const MAGIC: [u8; 8] = *b"ONELF\x00\x01\x00";
pub const END_MAGIC: [u8; 8] = *b"FLENONE\x00";

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Flags: u16 {
        const HAS_DICT       = 1 << 0;
        const MEMFD_HINT     = 1 << 1;
        const SHARUN_COMPAT  = 1 << 2;
    }
}

#[derive(Debug, Clone)]
pub struct Footer {
    /// Format version number (currently 1).
    pub format_version: u16,
    /// Feature flags describing optional sections and capabilities.
    pub flags: Flags,
    /// Byte offset where the compressed manifest begins.
    pub manifest_offset: u64,
    /// Size of the manifest after compression.
    pub manifest_compressed: u64,
    /// Size of the manifest before compression.
    pub manifest_original: u64,
    /// Byte offset where the payload section begins.
    pub payload_offset: u64,
    /// Total size of the payload section in bytes.
    pub payload_size: u64,
    /// Byte offset of the zstd dictionary, or 0 if absent.
    pub dict_offset: u64,
    /// Size of the zstd dictionary in bytes, or 0 if absent.
    pub dict_size: u32,
    /// xxHash32 checksum of the compressed manifest for integrity verification.
    pub manifest_checksum: [u8; 4],
}

impl Footer {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&MAGIC)?; // 8
        w.write_all(&self.format_version.to_le_bytes())?; // 2
        w.write_all(&self.flags.bits().to_le_bytes())?; // 2
        w.write_all(&self.manifest_offset.to_le_bytes())?; // 8
        w.write_all(&self.manifest_compressed.to_le_bytes())?; // 8
        w.write_all(&self.manifest_original.to_le_bytes())?; // 8
        w.write_all(&self.payload_offset.to_le_bytes())?; // 8
        w.write_all(&self.payload_size.to_le_bytes())?; // 8
        w.write_all(&self.dict_offset.to_le_bytes())?; // 8
        w.write_all(&self.dict_size.to_le_bytes())?; // 4
        w.write_all(&self.manifest_checksum)?; // 4
        w.write_all(&END_MAGIC)?; // 8
        Ok(()) // = 76
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut buf = [0u8; FOOTER_SIZE];
        r.read_exact(&mut buf)?;
        Self::from_bytes(&buf)
    }

    pub fn from_bytes(buf: &[u8; FOOTER_SIZE]) -> io::Result<Self> {
        if &buf[0..8] != &MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid onelf magic",
            ));
        }
        if &buf[68..76] != &END_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid onelf end magic",
            ));
        }

        let format_version = u16::from_le_bytes(buf[8..10].try_into().unwrap());
        if format_version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported format version: {}", format_version),
            ));
        }

        let flags_raw = u16::from_le_bytes(buf[10..12].try_into().unwrap());
        let flags = Flags::from_bits_truncate(flags_raw);

        Ok(Footer {
            format_version,
            flags,
            manifest_offset: u64::from_le_bytes(buf[12..20].try_into().unwrap()),
            manifest_compressed: u64::from_le_bytes(buf[20..28].try_into().unwrap()),
            manifest_original: u64::from_le_bytes(buf[28..36].try_into().unwrap()),
            payload_offset: u64::from_le_bytes(buf[36..44].try_into().unwrap()),
            payload_size: u64::from_le_bytes(buf[44..52].try_into().unwrap()),
            dict_offset: u64::from_le_bytes(buf[52..60].try_into().unwrap()),
            dict_size: u32::from_le_bytes(buf[60..64].try_into().unwrap()),
            manifest_checksum: buf[64..68].try_into().unwrap(),
        })
    }
}
