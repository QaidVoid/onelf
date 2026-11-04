//! Minimal ELF probing shared by the packer and the runtime.
//!
//! Only what both sides need to locate a program header, kept in one place so
//! a hardened parse cannot drift back into an unchecked duplicate.

use std::io;

/// One program header's file-relevant fields, normalized across ELF classes.
struct Phdr {
    p_type: u32,
    p_offset: u64,
    p_filesz: u64,
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..at + 2)
        .and_then(|b| <[u8; 2]>::try_from(b).ok())
        .map(u16::from_le_bytes)
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map(u64::from_le_bytes)
}

/// The ELF class and the program-header table's `(offset, entry size, count)`.
/// The offset truncates to `usize` like the probes that consume it.
fn phdr_table(data: &[u8]) -> Option<(u8, usize, usize, usize)> {
    if data.len() < 64 || data[0..4] != *b"\x7fELF" {
        return None;
    }
    let class = data[4];
    let (e_phoff, e_phentsize, e_phnum) = match class {
        2 => (
            u64_at(data, 32)? as usize,
            usize::from(u16_at(data, 54)?),
            usize::from(u16_at(data, 56)?),
        ),
        1 => (
            u32_at(data, 28)? as usize,
            usize::from(u16_at(data, 42)?),
            usize::from(u16_at(data, 44)?),
        ),
        _ => return None,
    };
    Some((class, e_phoff, e_phentsize, e_phnum))
}

/// Visit every program header fully contained in `data`, in file order.
/// Little-endian 32- and 64-bit ELF. A header table pointing outside
/// `data`, or an entry size too small to hold the fields read here, ends
/// the walk silently; callers treat that as "nothing found".
fn each_phdr(data: &[u8], mut visit: impl FnMut(&Phdr)) {
    let Some((class, e_phoff, e_phentsize, e_phnum)) = phdr_table(data) else {
        return;
    };
    // Each program-header entry must be large enough to hold the fields we
    // read below; reject malformed tables up front.
    if e_phentsize < if class == 2 { 56 } else { 32 } {
        return;
    }
    for i in 0..e_phnum {
        let Some(step) = i.checked_mul(e_phentsize) else {
            return;
        };
        let Some(off) = e_phoff.checked_add(step) else {
            return;
        };
        let Some(end) = off.checked_add(e_phentsize) else {
            return;
        };
        if end > data.len() {
            return;
        }
        let phdr = match class {
            2 => Phdr {
                p_type: match u32_at(data, off) {
                    Some(v) => v,
                    None => return,
                },
                p_offset: match u64_at(data, off + 8) {
                    Some(v) => v,
                    None => return,
                },
                p_filesz: match u64_at(data, off + 32) {
                    Some(v) => v,
                    None => return,
                },
            },
            1 => Phdr {
                p_type: match u32_at(data, off) {
                    Some(v) => v,
                    None => return,
                },
                p_offset: match u32_at(data, off + 4) {
                    Some(v) => u64::from(v),
                    None => return,
                },
                p_filesz: match u32_at(data, off + 16) {
                    Some(v) => u64::from(v),
                    None => return,
                },
            },
            _ => return,
        };
        visit(&phdr);
    }
}

/// Locate the PT_INTERP program header in `data` (which must contain the ELF
/// header and program-header table) and return the interpreter string's
/// `(file offset, size-in-file)`. Little-endian 32- and 64-bit ELF; every
/// read is bounds-checked against `data`. The interp bytes themselves may lie
/// beyond `data` (the caller reads them, possibly re-reading the file).
pub fn pt_interp_slot(data: &[u8]) -> Option<(usize, usize)> {
    let mut found: Option<(usize, usize)> = None;
    each_phdr(data, |ph| {
        if found.is_none() && ph.p_type == PT_INTERP {
            found = Some((ph.p_offset as usize, ph.p_filesz as usize));
        }
    });
    found
}

/// The `PT_INTERP` string of the ELF file at `path`, or `None` for a file
/// that is not an ELF object or names no interpreter.
///
/// The first 8 KB cover the header and program-header table; the string
/// itself is read at its own offset, which `patchelf` can move far into
/// the file. Its length is bounded like a path, so a corrupt entry cannot
/// drive a large allocation.
pub fn read_interp(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const MAX_INTERP: usize = 4096;

    let mut file = std::fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut head = vec![0u8; 8192];
    let n = file.read(&mut head).ok()?;
    head.truncate(n);
    let (offset, size) = pt_interp_slot(&head)?;
    if size == 0 || size > MAX_INTERP {
        return None;
    }
    let end = offset.checked_add(size)?;
    if end as u64 > file_len {
        return None;
    }
    let bytes = match head.get(offset..end) {
        Some(s) => s.to_vec(),
        None => {
            file.seek(SeekFrom::Start(offset as u64)).ok()?;
            let mut buf = vec![0u8; size];
            file.read_exact(&mut buf).ok()?;
            buf
        }
    };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).ok().map(String::from)
}

/// Program header type of the interpreter string.
const PT_INTERP: u32 = 3;
/// Program header type holding notes.
const PT_NOTE: u32 = 4;

/// Vendor name carried by every onelf note, NUL terminated on the wire.
pub const NOTE_NAME: &[u8] = b"ONELF";
/// Note type of the capability note within the onelf vendor namespace.
pub const NOTE_TYPE: u32 = 1;
/// Payload metadata schema version the runtime understands, as advertised
/// by its capability note and required by callers about to emit metadata.
pub const RUNTIME_METADATA_VERSION: u16 = 1;
/// Serialized width of the complete note entry, name padding included.
pub const CAPABILITY_NOTE_BYTES: usize = 32;

/// Magic starting the note's description body.
const NOTE_DESC_MAGIC: &[u8; 4] = b"OCAP";
/// Serialized width of the description body.
const NOTE_DESC_BYTES: usize = 12;

bitflags! {
    /// Feature advertisements carried in the runtime's capability note.
    ///
    /// The note is a compatibility signal, not proof of authenticity: it
    /// lets packaging refuse a substituted runtime that would silently
    /// ignore metadata it does not understand.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RuntimeCapabilities: u32 {
        /// Resolves entrypoint-scoped set selections into the execution view.
        const SELECTION_VIEW = 1 << 0;
        /// Imports pinned set artifacts from permitted sources into the store.
        const STORE_IMPORT = 1 << 1;
    }
}

impl RuntimeCapabilities {
    /// Every flag this decoder names; a set bit outside this mask is a
    /// decode error, so flipped bytes cannot forge an advertisement.
    pub const ALL_KNOWN: Self = Self::SELECTION_VIEW.union(Self::STORE_IMPORT);

    /// Decode raw bits, rejecting any bit without a named flag.
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::ALL_KNOWN.bits() != 0 {
            None
        } else {
            Some(Self { bits })
        }
    }

    /// Whether no flag is set.
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// The flags of `self` that `other` does not carry.
    pub const fn difference(self, other: Self) -> Self {
        Self {
            bits: self.bits & !other.bits,
        }
    }
}

impl std::fmt::Display for RuntimeCapabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return f.write_str("(none)");
        }
        let mut first = true;
        for (flag, name) in [
            (Self::SELECTION_VIEW, "selection-view"),
            (Self::STORE_IMPORT, "store-import"),
        ] {
            if self.contains(flag) {
                if !first {
                    f.write_str("+")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// A bounded diagnostic for a capability note that cannot be accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityNoteError(String);

impl CapabilityNoteError {
    fn new(reason: &str) -> Self {
        Self(reason.chars().take(768).collect())
    }
}

impl std::fmt::Display for CapabilityNoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CapabilityNoteError {}

impl From<CapabilityNoteError> for io::Error {
    fn from(value: CapabilityNoteError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, value.0)
    }
}

/// What a probed runtime advertises about its own capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeNote {
    /// A well-formed note: the runtime fully understands payload metadata
    /// up to `metadata_version` and carries `capabilities`.
    Present {
        /// Highest payload metadata schema version the runtime understands.
        metadata_version: u16,
        /// Named features this build supports.
        capabilities: RuntimeCapabilities,
    },
    /// The object is a valid ELF with no onelf note: a runtime from before
    /// capability signaling, usable only through its documented legacy path.
    Absent,
    /// A note claimed the onelf vendor name and capability type but cannot
    /// be accepted, so the runtime's true support is unknowable.
    Incompatible(CapabilityNoteError),
}

/// The metadata version and capabilities a consumer requires of a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeRequirements {
    /// Payload metadata schema version that must be advertised; zero
    /// requires nothing.
    pub metadata_version: u16,
    /// Capability flags that must all be advertised.
    pub capabilities: RuntimeCapabilities,
}

impl RuntimeRequirements {
    /// No metadata and no capabilities: anything embeddable passes.
    pub const NONE: Self = Self {
        metadata_version: 0,
        capabilities: RuntimeCapabilities::empty(),
    };

    /// Require support for one payload metadata schema version.
    pub const fn metadata(version: u16) -> Self {
        Self {
            metadata_version: version,
            capabilities: RuntimeCapabilities::empty(),
        }
    }

    /// Whether nothing at all is required.
    pub const fn is_none(self) -> bool {
        self.metadata_version == 0 && self.capabilities.is_empty()
    }
}

/// Encode the canonical capability note entry: namesz, descsz, type, the
/// NUL padded vendor name, then a description of magic [`NOTE_DESC_MAGIC`],
/// little-endian `u16` metadata version, zero `u16` reserved field, and
/// little-endian `u32` capability bits. `metadata_version` of zero encodes
/// fine but never decodes.
pub const fn encode_capability_note(
    metadata_version: u16,
    capabilities: RuntimeCapabilities,
) -> [u8; CAPABILITY_NOTE_BYTES] {
    let mut out = [0u8; CAPABILITY_NOTE_BYTES];
    let namesz = ((NOTE_NAME.len() + 1) as u32).to_le_bytes();
    let descsz = (NOTE_DESC_BYTES as u32).to_le_bytes();
    let ntype = NOTE_TYPE.to_le_bytes();
    let version = metadata_version.to_le_bytes();
    let bits = capabilities.bits().to_le_bytes();
    let mut i = 0;
    while i < 4 {
        out[i] = namesz[i];
        out[4 + i] = descsz[i];
        out[8 + i] = ntype[i];
        out[28 + i] = bits[i];
        i += 1;
    }
    i = 0;
    while i < NOTE_NAME.len() {
        out[12 + i] = NOTE_NAME[i];
        i += 1;
    }
    i = 0;
    while i < NOTE_DESC_MAGIC.len() {
        out[20 + i] = NOTE_DESC_MAGIC[i];
        i += 1;
    }
    i = 0;
    while i < version.len() {
        out[24 + i] = version[i];
        i += 1;
    }
    out
}

/// Decode one canonical note description body.
fn decode_note_desc(desc: &[u8]) -> Result<(u16, RuntimeCapabilities), CapabilityNoteError> {
    if desc.len() != NOTE_DESC_BYTES {
        return Err(CapabilityNoteError::new(&format!(
            "capability note description is {} bytes, expected {NOTE_DESC_BYTES}",
            desc.len()
        )));
    }
    if &desc[0..4] != NOTE_DESC_MAGIC {
        return Err(CapabilityNoteError::new(
            "capability note description has an unknown magic",
        ));
    }
    let metadata_version = u16::from_le_bytes([desc[4], desc[5]]);
    let reserved = u16::from_le_bytes([desc[6], desc[7]]);
    let bits = u32::from_le_bytes([desc[8], desc[9], desc[10], desc[11]]);
    if metadata_version == 0 {
        return Err(CapabilityNoteError::new(
            "capability note advertises metadata version 0",
        ));
    }
    if reserved != 0 {
        return Err(CapabilityNoteError::new(
            "capability note reserved field is not zero",
        ));
    }
    let Some(capabilities) = RuntimeCapabilities::from_bits(bits) else {
        return Err(CapabilityNoteError::new(&format!(
            "capability note advertises unknown features beyond {}",
            RuntimeCapabilities::ALL_KNOWN
        )));
    };
    Ok((metadata_version, capabilities))
}

/// Scan one PT_NOTE segment for the onelf capability note, updating `state`
/// on a first match.
///
/// Entries are located by vendor signature at every 4-byte stride instead of
/// a sequential walk, because linkers merge note sections with differing
/// internal alignment into one segment and a fixed-stride walk desyncs on
/// the padding between them. The description is accepted at the 4- and
/// 8-byte padded name offsets, whichever lies in bounds. Once the vendor
/// name and type match, a description that is not exactly the canonical
/// width or extends past the segment makes the note incompatible.
fn scan_note_segment(data: &[u8], ph: &Phdr, state: &mut RuntimeNote) {
    if !matches!(state, RuntimeNote::Absent) {
        return;
    }
    let start = ph.p_offset as usize;
    if start > data.len() {
        return;
    }
    let Some(seg_end) = start.checked_add(ph.p_filesz as usize) else {
        return;
    };
    if seg_end > data.len() {
        return;
    }
    let mut pos = start;
    while pos + 12 <= seg_end {
        let namesz = u32_at(data, pos);
        let descsz = u32_at(data, pos + 4);
        let ntype = u32_at(data, pos + 8);
        let candidate = pos;
        pos += 4;
        let (Some(namesz), Some(descsz), Some(ntype)) = (namesz, descsz, ntype) else {
            return;
        };
        if namesz as usize != NOTE_NAME.len() + 1
            || ntype != NOTE_TYPE
            || candidate + 12 + NOTE_NAME.len() + 1 > seg_end
            || data[candidate + 12..candidate + 12 + NOTE_NAME.len()] != *NOTE_NAME
            || data[candidate + 12 + NOTE_NAME.len()] != 0
        {
            continue;
        }
        if descsz as usize != NOTE_DESC_BYTES {
            *state = RuntimeNote::Incompatible(CapabilityNoteError::new(&format!(
                "capability note declares a {descsz}-byte description, expected {NOTE_DESC_BYTES}"
            )));
            return;
        }
        let mut rejected: Option<CapabilityNoteError> = None;
        for desc_pos in [candidate + 20, candidate + 24] {
            if desc_pos + NOTE_DESC_BYTES > seg_end {
                continue;
            }
            match decode_note_desc(&data[desc_pos..desc_pos + NOTE_DESC_BYTES]) {
                Ok((metadata_version, capabilities)) => {
                    *state = RuntimeNote::Present {
                        metadata_version,
                        capabilities,
                    };
                    return;
                }
                Err(e) => {
                    rejected.get_or_insert(e);
                }
            }
        }
        *state = RuntimeNote::Incompatible(rejected.unwrap_or_else(|| {
            CapabilityNoteError::new("capability note description is truncated")
        }));
        return;
    }
}

/// Probe runtime bytes for the onelf capability note.
///
/// Returns [`RuntimeNote::Absent`] for a valid ELF carrying no such note and
/// [`RuntimeNote::Incompatible`] when a note claims this vendor and type but
/// is truncated, malformed, or unsupported, so absence and corruption stay
/// distinguishable. Returns an error when `data` is not a parseable
/// little-endian 32- or 64-bit ELF object. The scan is bounded by each
/// segment's declared extent; no declared length drives an allocation.
pub fn probe_capability_note(data: &[u8]) -> Result<RuntimeNote, CapabilityNoteError> {
    let Some((class, _, _, _)) = phdr_table(data) else {
        return Err(CapabilityNoteError::new(
            "runtime is not a little-endian 32- or 64-bit ELF object",
        ));
    };
    if class != 1 && class != 2 {
        return Err(CapabilityNoteError::new(&format!(
            "runtime ELF has unknown class {class}"
        )));
    }
    let mut state = RuntimeNote::Absent;
    each_phdr(data, |ph| {
        if ph.p_type == PT_NOTE {
            scan_note_segment(data, ph, &mut state);
        }
    });
    Ok(state)
}

/// Decide whether a probed runtime satisfies `required`.
///
/// An [`RuntimeNote::Absent`] runtime passes only when nothing is required,
/// which is the documented legacy path for packages without metadata. A
/// runtime whose note is [`RuntimeNote::Incompatible`] is always refused,
/// because its advertisement cannot be trusted even for an empty
/// requirement. Otherwise the advertised metadata version must be at least
/// `required.metadata_version` and every required capability must be set.
pub fn check_runtime_support(
    note: &RuntimeNote,
    required: RuntimeRequirements,
) -> Result<(), CapabilityNoteError> {
    match note {
        RuntimeNote::Incompatible(e) => Err(e.clone()),
        RuntimeNote::Present {
            metadata_version,
            capabilities,
        } => {
            if *metadata_version < required.metadata_version {
                return Err(CapabilityNoteError::new(&format!(
                    "runtime advertises payload metadata version {metadata_version}, \
                     packaging requires {required_value}",
                    required_value = required.metadata_version
                )));
            }
            let missing = required.capabilities.difference(*capabilities);
            if !missing.is_empty() {
                return Err(CapabilityNoteError::new(&format!(
                    "runtime does not advertise the required {missing} support"
                )));
            }
            Ok(())
        }
        RuntimeNote::Absent => {
            if required.is_none() {
                return Ok(());
            }
            let mut what = String::new();
            if required.metadata_version != 0 {
                what.push_str(&format!("metadata version {}", required.metadata_version));
            }
            if !required.capabilities.is_empty() {
                if !what.is_empty() {
                    what.push_str(" and ");
                }
                what.push_str(&required.capabilities.to_string());
            }
            Err(CapabilityNoteError::new(&format!(
                "runtime carries no capability note, so it does not support {what}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-bit ELF whose single program header is a PT_INTERP pointing at
    /// `interp`, laid out immediately after the header table.
    fn elf64_with_interp(interp: &str) -> Vec<u8> {
        let phoff = 64usize;
        let phentsize = 56usize;
        let interp_off = phoff + phentsize;
        let mut v = vec![0u8; interp_off + interp.len() + 1];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // ELFCLASS64
        v[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        v[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes());
        v[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        v[phoff..phoff + 4].copy_from_slice(&3u32.to_le_bytes()); // PT_INTERP
        v[phoff + 8..phoff + 16].copy_from_slice(&(interp_off as u64).to_le_bytes());
        v[phoff + 32..phoff + 40].copy_from_slice(&((interp.len() + 1) as u64).to_le_bytes());
        v[interp_off..interp_off + interp.len()].copy_from_slice(interp.as_bytes());
        v
    }

    #[test]
    fn locates_the_interp_slot() {
        let name = "/lib64/ld-linux-x86-64.so.2";
        let elf = elf64_with_interp(name);
        let (off, sz) = pt_interp_slot(&elf).expect("slot");
        assert_eq!(off, 120);
        assert_eq!(sz, name.len() + 1);
        assert_eq!(&elf[off..off + sz - 1], name.as_bytes());
    }

    #[test]
    fn malformed_returns_none_without_panic() {
        assert!(pt_interp_slot(b"not an elf").is_none());
        assert!(pt_interp_slot(&[]).is_none());

        // ELF magic but a program-header table pointing off the end.
        let mut short = vec![0u8; 64];
        short[0..4].copy_from_slice(b"\x7fELF");
        short[4] = 2;
        short[56..58].copy_from_slice(&9999u16.to_le_bytes());
        short[32..40].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert!(pt_interp_slot(&short).is_none());
    }

    #[test]
    fn undersized_phentsize_is_rejected() {
        // A table whose entries are too small to hold the fields read below
        // would otherwise be walked with garbage offsets.
        let mut elf = elf64_with_interp("/lib/ld.so");
        elf[54..56].copy_from_slice(&8u16.to_le_bytes());
        assert!(pt_interp_slot(&elf).is_none());
    }
}
