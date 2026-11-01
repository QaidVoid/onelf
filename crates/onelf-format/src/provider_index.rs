//! Verified provider index of a pinned set artifact.
//!
//! A pinned set artifact is a shared dependency build identified by its
//! set declaration name and its full BLAKE3 pin. This index records the
//! loader-namespace names (sonames) that the artifact's shared objects
//! verifiably export. It is packed inside the artifact it describes, so
//! its bytes are covered by the artifact's own hash: the index travels
//! with, and is bound to, exactly the build it names.
//!
//! Wire layout, all integers little-endian:
//!
//! ```text
//! 0..4    magic "OPRV"
//! 4..6    u16 version, 1
//! 6..8    u16 reserved flags, zero
//! 8..12   u32 payload length, bytes after the 16-byte header
//! 12..16  u32 record count, the identity record plus every exported name
//! 16..    records
//! ```
//!
//! A record is a u32 body length followed by a u8 tag and the tag's
//! fields. The identity record (tag 1) carries the set declaration name
//! and comes first, exactly once. Each exported-name record (tag 2)
//! carries one soname. Exported names are sorted by bytes and unique, so
//! decoding rejects duplicates and noncanonical order, and every accepted
//! byte string re-encodes byte-for-byte.
//!
//! Bounds: a file of at most [`MAX_INDEX_BYTES`], at most [`MAX_RECORDS`]
//! records, and names of one to 128 UTF-8 bytes. A set name is ASCII
//! letters, digits, `-` and `_`. An exported name never contains NUL or
//! `/`, which rules out names the loader could not resolve beside the
//! file.
//!
//! Providers are distinguished by artifact identity: two artifacts that
//! export the same name are different providers, because each index is
//! bound to its own full pin. A file that merely shares a basename with
//! an exported name without exporting that soname, a private plugin say,
//! produces no record, and consumers must not treat it as a provider on
//! that string alone.
//!
//! Consumers MUST verify that the bytes an index was decoded from hash to
//! the pin they expect before trusting its exports; [`ProviderIndex::verify`]
//! completes that binding given the full digest of the carrying artifact.
//! An artifact without an index keeps its documented legacy behavior: its
//! recorded provision listing is used as before and no republishing is
//! needed. A present but malformed index is a bounded failure, never a
//! silent fallback to legacy behavior. The runtime does not read this
//! format yet; [`ProviderIndex::decode`] is its intended entry point when
//! it does.

use std::fmt;

use crate::rules::{MAX_ID_BYTES, MAX_RECORDS};

/// Format version this build writes and reads.
pub const VERSION: u16 = 1;

/// Encoded header size.
pub const HEADER_BYTES: usize = 16;

/// Maximum complete encoded index size, including its header.
pub const MAX_INDEX_BYTES: usize = 1024 * 1024;

/// A bounded diagnostic for a malformed or unsupported index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderIndexError(String);

impl ProviderIndexError {
    fn new(reason: &str) -> Self {
        Self(reason.chars().take(768).collect())
    }
}

impl fmt::Display for ProviderIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProviderIndexError {}

/// The pinned set artifact an index belongs to: the set declaration name
/// and the full BLAKE3 digest of the artifact bytes that carry the index.
///
/// The pair is a provider identity. Two artifacts with the same
/// declaration name but different pins are different providers, and the
/// same name exported by both is a provider difference, not a duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIdentity {
    /// Set declaration name, as validated by [`ProviderIndex::new`].
    pub set: String,
    /// Full BLAKE3 digest of the artifact that carries the index.
    pub blake3: [u8; 32],
}

/// The loader-namespace names one pinned set artifact verifiably exports.
///
/// Every export was derived from the `DT_SONAME` of an object packed in
/// the artifact. Files that export no soname are absent, whatever their
/// basenames read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderIndex {
    /// Set declaration name the index describes.
    pub set: String,
    /// Exported sonames, sorted by bytes and unique.
    pub exports: Vec<String>,
}

impl ProviderIndex {
    /// Build an index for the set `name` and its verified exports.
    ///
    /// The exports are sorted into canonical order; duplicate names are
    /// refused rather than collapsed, since a caller that reports one name
    /// twice has derived it wrongly.
    pub fn new(set: &str, exports: &[String]) -> Result<Self, ProviderIndexError> {
        validate_set_name(set)?;
        let mut sorted = exports.to_vec();
        sorted.sort();
        let value = Self {
            set: set.to_owned(),
            exports: sorted,
        };
        value.validate()?;
        Ok(value)
    }

    /// Check internal bounds, name rules and canonical export order.
    fn validate(&self) -> Result<(), ProviderIndexError> {
        validate_set_name(&self.set)?;
        if self.exports.len() + 1 > MAX_RECORDS {
            return Err(ProviderIndexError::new(
                "exports: identity plus names exceeds MAX_RECORDS (4096)",
            ));
        }
        for name in &self.exports {
            validate_export_name(name)?;
        }
        if self.exports.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(ProviderIndexError::new(
                "exports: names must be sorted and unique",
            ));
        }
        Ok(())
    }

    /// Require this index to describe exactly `identity`.
    ///
    /// The caller must already have verified that the bytes the index was
    /// decoded from hash to `identity.blake3`; this check completes the
    /// binding by refusing an index that names any other set.
    pub fn verify(&self, identity: &ArtifactIdentity) -> Result<(), ProviderIndexError> {
        if self.set != identity.set {
            return Err(ProviderIndexError::new(&format!(
                "provider index: names set {}, not {}",
                self.set, identity.set
            )));
        }
        Ok(())
    }

    /// Encode canonical index bytes.
    pub fn encode(&self) -> Result<Vec<u8>, ProviderIndexError> {
        self.validate()?;
        let mut writer = Writer::new(self.exports.len() + 1)?;
        let start = writer.start_record(1)?;
        writer.string(&self.set)?;
        writer.end_record(start)?;
        for name in &self.exports {
            let start = writer.start_record(2)?;
            writer.string(name)?;
            writer.end_record(start)?;
        }
        writer.finish()
    }

    /// Decode a present index, rejecting malformed, truncated, duplicate
    /// or noncanonical input. Total over arbitrary bytes: hostile
    /// lengths and counts error instead of panicking or over-allocating.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProviderIndexError> {
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(ProviderIndexError::new(
                "file: exceeds MAX_INDEX_BYTES (1 MiB)",
            ));
        }
        let mut reader = Reader { bytes, offset: 0 };
        if reader.take(4)? != *b"OPRV" {
            return Err(ProviderIndexError::new("header: invalid index magic"));
        }
        let version = reader.take(2)?;
        if u16::from_le_bytes([version[0], version[1]]) != VERSION {
            return Err(ProviderIndexError::new("header: unsupported index version"));
        }
        if reader.take(2)? != [0, 0] {
            return Err(ProviderIndexError::new("header: unknown reserved flags"));
        }
        let payload = reader.u32()?;
        let records = reader.u32()?;
        if records > MAX_RECORDS {
            return Err(ProviderIndexError::new(
                "header: exceeds MAX_RECORDS (4096)",
            ));
        }
        if payload != bytes.len() - HEADER_BYTES {
            return Err(ProviderIndexError::new(
                "header payload length: inconsistent with complete file",
            ));
        }
        let mut body = Reader {
            bytes: &bytes[HEADER_BYTES..],
            offset: 0,
        };
        let mut set: Option<String> = None;
        let mut exports = Vec::new();
        for _ in 0..records {
            let (tag, mut record) = body.record()?;
            match tag {
                1 => {
                    if set.is_some() {
                        return Err(ProviderIndexError::new("identity: duplicate record"));
                    }
                    set = Some(record.string(MAX_ID_BYTES)?.to_owned());
                }
                2 => {
                    if set.is_none() {
                        return Err(ProviderIndexError::new(
                            "exports: the identity record must come first",
                        ));
                    }
                    exports.push(record.string(MAX_ID_BYTES)?.to_owned());
                }
                _ => {
                    return Err(ProviderIndexError::new(
                        "record: unknown or misplaced variant tag",
                    ));
                }
            }
            record.finish()?;
        }
        body.finish()?;
        let value = Self {
            set: set.ok_or_else(|| ProviderIndexError::new("identity: record missing"))?,
            exports,
        };
        value.validate()?;
        Ok(value)
    }
}

/// Require a set declaration name of one to 128 ASCII letters, digits,
/// `-` and `_`, the same rule the packer applies to set names everywhere.
fn validate_set_name(name: &str) -> Result<(), ProviderIndexError> {
    if name.is_empty()
        || name.len() > MAX_ID_BYTES
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ProviderIndexError::new(
            "set name: requires 1..=128 ASCII letters, digits, - and _",
        ));
    }
    Ok(())
}

/// Require an exported name of one to 128 UTF-8 bytes with no NUL and no
/// `/`: a name naming a directory of its own is not one the loader
/// resolves beside the file.
fn validate_export_name(name: &str) -> Result<(), ProviderIndexError> {
    if name.is_empty() || name.len() > MAX_ID_BYTES || name.contains(['\0', '/']) {
        return Err(ProviderIndexError::new(
            "exported name: requires 1..=128 UTF-8 bytes without NUL or /",
        ));
    }
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], ProviderIndexError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| ProviderIndexError::new("length: arithmetic overflow"))?;
        let value = self.bytes.get(self.offset..end).ok_or_else(|| {
            ProviderIndexError::new("index: truncated field or length exceeds remaining bytes")
        })?;
        self.offset = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, ProviderIndexError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<usize, ProviderIndexError> {
        let value = self.take(4)?;
        Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]) as usize)
    }

    fn record(&mut self) -> Result<(u8, Reader<'a>), ProviderIndexError> {
        let count = self.u32()?;
        if count == 0 {
            return Err(ProviderIndexError::new(
                "record length: must include a known tag",
            ));
        }
        let mut record = Reader {
            bytes: self.take(count)?,
            offset: 0,
        };
        let tag = record.byte()?;
        Ok((tag, record))
    }

    fn string(&mut self, maximum: usize) -> Result<&'a str, ProviderIndexError> {
        let count = self.u32()?;
        if count == 0 || count > maximum {
            return Err(ProviderIndexError::new(
                "name length: requires 1..=128 bytes",
            ));
        }
        std::str::from_utf8(self.take(count)?)
            .map_err(|_| ProviderIndexError::new("string: invalid UTF-8"))
    }

    fn finish(&self) -> Result<(), ProviderIndexError> {
        if self.offset != self.bytes.len() {
            return Err(ProviderIndexError::new(
                "index: trailing bytes in file or record",
            ));
        }
        Ok(())
    }
}

struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn new(records: usize) -> Result<Self, ProviderIndexError> {
        let mut writer = Self { bytes: Vec::new() };
        writer.bytes(b"OPRV")?;
        writer.bytes(&VERSION.to_le_bytes())?;
        writer.bytes(&[0, 0])?;
        writer.count(0)?;
        writer.count(records)?;
        Ok(writer)
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<(), ProviderIndexError> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > MAX_INDEX_BYTES)
        {
            return Err(ProviderIndexError::new(
                "file: exceeds MAX_INDEX_BYTES (1 MiB)",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn byte(&mut self, value: u8) -> Result<(), ProviderIndexError> {
        self.bytes(&[value])
    }

    fn count(&mut self, count: usize) -> Result<(), ProviderIndexError> {
        let count =
            u32::try_from(count).map_err(|_| ProviderIndexError::new("length: exceeds u32"))?;
        self.bytes(&count.to_le_bytes())
    }

    fn string(&mut self, value: &str) -> Result<(), ProviderIndexError> {
        self.count(value.len())?;
        self.bytes(value.as_bytes())
    }

    fn start_record(&mut self, tag: u8) -> Result<usize, ProviderIndexError> {
        let start = self.bytes.len();
        self.count(0)?;
        self.byte(tag)?;
        Ok(start)
    }

    fn end_record(&mut self, start: usize) -> Result<(), ProviderIndexError> {
        let count = u32::try_from(self.bytes.len() - start - 4)
            .map_err(|_| ProviderIndexError::new("record length: exceeds u32"))?;
        self.bytes[start..start + 4].copy_from_slice(&count.to_le_bytes());
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, ProviderIndexError> {
        let count = u32::try_from(self.bytes.len() - HEADER_BYTES)
            .map_err(|_| ProviderIndexError::new("payload length: exceeds u32"))?;
        self.bytes[8..12].copy_from_slice(&count.to_le_bytes());
        Ok(self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> ProviderIndex {
        ProviderIndex {
            set: "gtk".to_owned(),
            exports: vec!["libgio-2.0.so.0".to_owned(), "libgtk-3.so.0".to_owned()],
        }
    }

    #[test]
    fn encoding_is_byte_exact() {
        let bytes = index().encode().unwrap();
        let hex = "4f505256010000003a0000000300000008000000010300000067746b14000000020f0000006c696267696f2d322e302e736f2e3012000000020d0000006c696267746b2d332e736f2e30";
        let expected: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(bytes, expected, "the wire layout is pinned");
    }

    #[test]
    fn roundtrip_reencodes_byte_for_byte() {
        let bytes = index().encode().unwrap();
        let back = ProviderIndex::decode(&bytes).unwrap();
        assert_eq!(back, index());
        assert_eq!(back.encode().unwrap(), bytes);
    }

    #[test]
    fn new_sorts_exports_and_rejects_duplicates() {
        let value =
            ProviderIndex::new("qt", &["libb.so.1".to_owned(), "liba.so.2".to_owned()]).unwrap();
        assert_eq!(value.exports, ["liba.so.2", "libb.so.1"]);
        assert!(
            ProviderIndex::new("qt", &["liba.so.1".to_owned(), "liba.so.1".to_owned()]).is_err()
        );
    }

    #[test]
    fn empty_exports_are_structurally_valid() {
        let value = ProviderIndex::new("docs", &[]).unwrap();
        let back = ProviderIndex::decode(&value.encode().unwrap()).unwrap();
        assert_eq!(back, value);
        assert!(back.exports.is_empty());
    }

    #[test]
    fn set_name_rules_match_the_packer() {
        for ok in ["gtk", "a", "A-b_1"] {
            assert!(validate_set_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "qt6.core", "/abs", "a b", "sp ace", &"x".repeat(129)] {
            assert!(validate_set_name(bad).is_err(), "{bad}");
        }
        let longest = "x".repeat(128);
        assert!(validate_set_name(&longest).is_ok());
    }

    #[test]
    fn export_name_rules() {
        assert!(validate_export_name("libfoo.so.1").is_ok());
        assert!(validate_export_name("libc.musl-x86_64.so.1").is_ok());
        for bad in ["", "a/b.so", "a\0b", &"x".repeat(129)] {
            assert!(validate_export_name(bad).is_err(), "{bad}");
        }
        assert!(validate_export_name(&"x".repeat(128)).is_ok());
    }

    /// Encode with the identity record after the exports, and with the
    /// identity record twice: neither is canonical.
    #[test]
    fn a_misplaced_or_duplicate_identity_is_refused() {
        let mut writer = Writer::new(2).unwrap();
        let start = writer.start_record(2).unwrap();
        writer.string("liba.so.1").unwrap();
        writer.end_record(start).unwrap();
        let start = writer.start_record(1).unwrap();
        writer.string("qt").unwrap();
        writer.end_record(start).unwrap();
        let late = writer.finish().unwrap();
        assert!(ProviderIndex::decode(&late).is_err(), "identity not first");

        let mut writer = Writer::new(2).unwrap();
        let start = writer.start_record(1).unwrap();
        writer.string("qt").unwrap();
        writer.end_record(start).unwrap();
        let start = writer.start_record(1).unwrap();
        writer.string("qt").unwrap();
        writer.end_record(start).unwrap();
        let twice = writer.finish().unwrap();
        assert!(ProviderIndex::decode(&twice).is_err(), "identity twice");
    }

    #[test]
    fn an_unknown_tag_is_refused() {
        let mut writer = Writer::new(2).unwrap();
        let start = writer.start_record(1).unwrap();
        writer.string("qt").unwrap();
        writer.end_record(start).unwrap();
        let start = writer.start_record(3).unwrap();
        writer.string("liba.so.1").unwrap();
        writer.end_record(start).unwrap();
        assert!(ProviderIndex::decode(&writer.finish().unwrap()).is_err());
    }

    #[test]
    fn unsorted_and_duplicate_exports_are_refused() {
        for exports in [
            vec!["libb.so.1", "liba.so.1"],
            vec!["liba.so.1", "liba.so.1"],
        ] {
            let mut writer = Writer::new(1 + exports.len()).unwrap();
            let start = writer.start_record(1).unwrap();
            writer.string("qt").unwrap();
            writer.end_record(start).unwrap();
            for name in &exports {
                let start = writer.start_record(2).unwrap();
                writer.string(name).unwrap();
                writer.end_record(start).unwrap();
            }
            let bytes = writer.finish().unwrap();
            assert!(ProviderIndex::decode(&bytes).is_err(), "{exports:?}");
        }
    }

    #[test]
    fn header_fields_are_checked() {
        let bytes = index().encode().unwrap();

        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(ProviderIndex::decode(&bad).is_err(), "magic");

        for version in [0u16, VERSION + 1] {
            let mut bad = bytes.clone();
            bad[4..6].copy_from_slice(&version.to_le_bytes());
            assert!(ProviderIndex::decode(&bad).is_err(), "version {version}");
        }

        let mut bad = bytes.clone();
        bad[6..8].copy_from_slice(&1u16.to_le_bytes());
        assert!(ProviderIndex::decode(&bad).is_err(), "reserved flags");

        let mut bad = bytes.clone();
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ProviderIndex::decode(&bad).is_err(), "payload length");

        let mut bad = bytes.clone();
        bad[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ProviderIndex::decode(&bad).is_err(), "record count");
    }

    #[test]
    fn truncation_and_trailing_bytes_are_refused() {
        let bytes = index().encode().unwrap();
        for end in 0..bytes.len() {
            assert!(
                ProviderIndex::decode(&bytes[..end]).is_err(),
                "truncated at {end}"
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(ProviderIndex::decode(&trailing).is_err(), "trailing byte");
    }

    /// Decoding is total: no hostile count, length or bitflip may panic
    /// or allocate beyond the input.
    #[test]
    fn hostile_input_never_panics() {
        let bytes = index().encode().unwrap();
        let mut hostile = Vec::new();
        hostile.extend_from_slice(&[0u8; HEADER_BYTES]);
        hostile.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(ProviderIndex::decode(&hostile).is_err());
        let mut hostile = bytes.clone();
        hostile[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ProviderIndex::decode(&hostile).is_err());

        for position in 0..bytes.len() {
            for flipped in [0x00u8, 0x7f, 0x80, 0xff] {
                let mut bad = bytes.clone();
                bad[position] = flipped;
                let _ = ProviderIndex::decode(&bad);
            }
        }
        let _ = ProviderIndex::decode(&[]);
        let _ = ProviderIndex::decode(&[0xff; HEADER_BYTES - 1]);
        let _ = ProviderIndex::decode(&vec![0xff; MAX_INDEX_BYTES + 1]);
    }

    #[test]
    fn the_record_cap_bounds_exports() {
        let too_many: Vec<String> = (0..MAX_RECORDS).map(|i| format!("lib{i}.so.1")).collect();
        assert!(ProviderIndex::new("qt", &too_many).is_err());
        let fitting: Vec<String> = (0..MAX_RECORDS - 1)
            .map(|i| format!("lib{i}.so.1"))
            .collect();
        let value = ProviderIndex::new("qt", &fitting).unwrap();
        assert_eq!(value.exports.len(), MAX_RECORDS - 1);
    }

    #[test]
    fn verify_binds_the_set_of_an_identity() {
        let value = index();
        let identity = ArtifactIdentity {
            set: "gtk".to_owned(),
            blake3: [7; 32],
        };
        assert!(value.verify(&identity).is_ok());
        let other = ArtifactIdentity {
            set: "qt".to_owned(),
            blake3: [7; 32],
        };
        assert!(value.verify(&other).is_err());
    }

    /// A file above the size bound is refused before any field is read.
    #[test]
    fn oversized_files_are_refused() {
        let value = ProviderIndex {
            set: "a".to_owned(),
            exports: Vec::new(),
        };
        let bytes = value.encode().unwrap();
        let mut padded = bytes;
        padded[8..12].copy_from_slice(&((MAX_INDEX_BYTES) as u32).to_le_bytes());
        padded.resize(MAX_INDEX_BYTES + 1, 0);
        assert!(ProviderIndex::decode(&padded).is_err());
    }
}
