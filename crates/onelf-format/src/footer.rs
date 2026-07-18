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
//! ```
//!
//! # Example
//!
//! ```ignore
//! use onelf_format::Footer;
//!
//! let footer = Footer {
//!     format_version: 1,
//!     // ... other fields
//! };
//! ```

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
        /// Payload blocks are stored raw (no zstd). `compressed_size`
        /// equals `original_size` for every block; the runtime reads
        /// payload bytes directly without decompression.
        const STORED         = 1 << 3;
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
    /// xxHash32 checksum of the *uncompressed* manifest bytes, verified
    /// before the manifest is trusted.
    pub manifest_checksum: [u8; 4],
}

impl Footer {
    /// Whether the payload is stored raw (no zstd). When true the runtime
    /// must skip decompression and use payload bytes directly.
    pub fn is_stored(&self) -> bool {
        self.flags.contains(Flags::STORED)
    }

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
        let flags = Flags::from_bits_retain(flags_raw);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Footer {
        Footer {
            format_version: 1,
            flags: Flags::HAS_DICT | Flags::STORED,
            manifest_offset: 0x1122,
            manifest_compressed: 0x33,
            manifest_original: 0x44,
            payload_offset: 0x55,
            payload_size: 0x66,
            dict_offset: 0x77,
            dict_size: 0x88,
            manifest_checksum: [1, 2, 3, 4],
        }
    }

    #[test]
    fn footer_roundtrips() {
        let f = sample();
        let mut buf = Vec::new();
        f.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), FOOTER_SIZE);
        let back = Footer::from_bytes(&buf.try_into().unwrap()).unwrap();
        assert_eq!(back.format_version, f.format_version);
        assert_eq!(back.flags, f.flags);
        assert_eq!(back.manifest_offset, f.manifest_offset);
        assert_eq!(back.dict_size, f.dict_size);
        assert_eq!(back.manifest_checksum, f.manifest_checksum);
    }

    #[test]
    fn malformed_footers_error_without_panic() {
        let mut buf = Vec::new();
        sample().write_to(&mut buf).unwrap();

        // Bad start magic.
        let mut bad = buf.clone();
        bad[0] ^= 0xff;
        assert!(Footer::from_bytes(&bad.clone().try_into().unwrap()).is_err());

        // Bad end magic.
        let mut bad = buf.clone();
        bad[68] ^= 0xff;
        assert!(Footer::from_bytes(&bad.clone().try_into().unwrap()).is_err());

        // Unsupported version.
        let mut bad = buf.clone();
        bad[8..10].copy_from_slice(&2u16.to_le_bytes());
        assert!(Footer::from_bytes(&bad.try_into().unwrap()).is_err());
    }
}
