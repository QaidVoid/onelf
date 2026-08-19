use std::collections::HashMap;
use std::io::{self, Cursor, Read, Write};
use std::path::PathBuf;

use crate::entry::{ENTRY_HEADER_SIZE, ENTRYPOINT_SIZE, Entry, EntryPoint};

/// Returns true if `name` is a single safe path component: non-empty,
/// not `.` or `..`, and free of `/` and NUL. Used to reject entry names
/// that would let a crafted package escape the extraction root.
pub fn is_safe_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// Lexically check that a symlink placed at `link_rel` (a relative path
/// under the extraction root) with `target` still resolves inside the
/// root. Absolute targets and targets that walk above the root are
/// rejected. Purely lexical, so it cannot be defeated by filesystem
/// races. Shared by the runtime and the packer's extractor.
pub fn symlink_target_within_root(link_rel: &std::path::Path, target: &str) -> bool {
    use std::ffi::OsString;
    use std::path::Component;

    if target.is_empty() {
        return false;
    }
    let tpath = std::path::Path::new(target);
    if tpath.is_absolute() {
        return false;
    }
    let mut stack: Vec<OsString> = Vec::new();
    let mut walk = |path: &std::path::Path| -> bool {
        for c in path.components() {
            match c {
                Component::Normal(s) => stack.push(s.to_os_string()),
                Component::ParentDir => {
                    if stack.pop().is_none() {
                        return false;
                    }
                }
                Component::CurDir => {}
                _ => return false,
            }
        }
        true
    };
    // Begin at the directory containing the symlink, then apply target.
    let parent_ok = link_rel.parent().map(&mut walk).unwrap_or(true);
    parent_ok && walk(tpath)
}

pub const MANIFEST_HEADER_SIZE: usize = 2 + 4 + 4 + 2 + 2 + 2 + 2 + 32; // 50 bytes

#[derive(Debug, Clone)]
pub struct ManifestHeader {
    /// Manifest format version.
    pub version: u16,
    /// Total number of filesystem entries in the manifest.
    pub entry_count: u32,
    /// Size of the string table in bytes.
    pub string_table_size: u32,
    /// Number of entrypoints defined in this package.
    pub entrypoint_count: u16,
    /// Index of the default entrypoint to use when none is specified.
    pub default_entrypoint: u16,
    /// Number of library directory paths in the manifest.
    pub lib_dir_count: u16,
    /// Offset into the string table for the package name.
    pub name_offset: u16,
    /// Unique package identifier (BLAKE3 hash).
    pub package_id: [u8; 32],
}

impl ManifestHeader {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.version.to_le_bytes())?;
        w.write_all(&self.entry_count.to_le_bytes())?;
        w.write_all(&self.string_table_size.to_le_bytes())?;
        w.write_all(&self.entrypoint_count.to_le_bytes())?;
        w.write_all(&self.default_entrypoint.to_le_bytes())?;
        w.write_all(&self.lib_dir_count.to_le_bytes())?;
        w.write_all(&self.name_offset.to_le_bytes())?;
        w.write_all(&self.package_id)?;
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut buf = [0u8; MANIFEST_HEADER_SIZE];
        r.read_exact(&mut buf)?;

        let mut package_id = [0u8; 32];
        package_id.copy_from_slice(&buf[18..50]);

        Ok(ManifestHeader {
            version: u16::from_le_bytes(buf[0..2].try_into().unwrap()),
            entry_count: u32::from_le_bytes(buf[2..6].try_into().unwrap()),
            string_table_size: u32::from_le_bytes(buf[6..10].try_into().unwrap()),
            entrypoint_count: u16::from_le_bytes(buf[10..12].try_into().unwrap()),
            default_entrypoint: u16::from_le_bytes(buf[12..14].try_into().unwrap()),
            lib_dir_count: u16::from_le_bytes(buf[14..16].try_into().unwrap()),
            name_offset: u16::from_le_bytes(buf[16..18].try_into().unwrap()),
            package_id,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Manifest {
    /// Fixed-size header containing counts, offsets, and the package ID.
    pub header: ManifestHeader,
    /// Named executable entrypoints into the package.
    pub entrypoints: Vec<EntryPoint>,
    /// All filesystem entries (files, directories, symlinks) in the package.
    pub entries: Vec<Entry>,
    /// Library directory string table offsets for `LD_LIBRARY_PATH` injection.
    pub lib_dir_offsets: Vec<u32>,
    /// Null-terminated string pool referenced by offset from entries and entrypoints.
    pub string_table: Vec<u8>,
}

impl Manifest {
    pub fn serialize(&self) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.header.write_to(&mut buf)?;
        for ep in &self.entrypoints {
            ep.write_to(&mut buf)?;
        }
        for entry in &self.entries {
            entry.write_to(&mut buf)?;
        }
        for &offset in &self.lib_dir_offsets {
            buf.write_all(&offset.to_le_bytes())?;
        }
        buf.write_all(&self.string_table)?;
        Ok(buf)
    }

    pub fn deserialize(data: &[u8]) -> io::Result<Self> {
        let total = data.len();
        let mut cursor = Cursor::new(data);
        let header = ManifestHeader::read_from(&mut cursor)?;

        if header.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported manifest version: {}", header.version),
            ));
        }

        // Clamp every speculative allocation to what the input can back,
        // so an oversized count in the header cannot request huge memory
        // before `read_exact` fails on the truncated data.
        let remaining = |c: &Cursor<&[u8]>| total.saturating_sub(c.position() as usize);

        let ep_cap = (header.entrypoint_count as usize).min(remaining(&cursor) / ENTRYPOINT_SIZE);
        let mut entrypoints = Vec::with_capacity(ep_cap);
        for _ in 0..header.entrypoint_count {
            entrypoints.push(EntryPoint::read_from(&mut cursor)?);
        }

        let entry_cap = (header.entry_count as usize).min(remaining(&cursor) / ENTRY_HEADER_SIZE);
        let mut entries = Vec::with_capacity(entry_cap);
        for _ in 0..header.entry_count {
            entries.push(Entry::read_from(&mut cursor)?);
        }

        let ld_cap = (header.lib_dir_count as usize).min(remaining(&cursor) / 4);
        let mut lib_dir_offsets = Vec::with_capacity(ld_cap);
        for _ in 0..header.lib_dir_count {
            let mut offset_buf = [0u8; 4];
            cursor.read_exact(&mut offset_buf)?;
            lib_dir_offsets.push(u32::from_le_bytes(offset_buf));
        }

        let st_size = header.string_table_size as usize;
        if st_size > remaining(&cursor) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "string table size exceeds remaining input",
            ));
        }
        let mut string_table = vec![0u8; st_size];
        cursor.read_exact(&mut string_table)?;

        let manifest = Manifest {
            header,
            entrypoints,
            entries,
            lib_dir_offsets,
            string_table,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate every cross-reference (parent, target_entry,
    /// default_entrypoint, string offsets) against its range. Rejects a
    /// crafted manifest before any consumer indexes it directly.
    fn validate(&self) -> io::Result<()> {
        let n = self.entries.len();
        let st = self.string_table.len() as u32;
        let bad = |msg: &'static str| io::Error::new(io::ErrorKind::InvalidData, msg);

        for e in &self.entries {
            if e.parent != u32::MAX && e.parent as usize >= n {
                return Err(bad("entry parent index out of range"));
            }
            if e.name > st {
                return Err(bad("entry name offset out of range"));
            }
            if e.symlink_target > st {
                return Err(bad("entry symlink_target offset out of range"));
            }
        }
        for ep in &self.entrypoints {
            if ep.target_entry as usize >= n {
                return Err(bad("entrypoint target_entry out of range"));
            }
            if ep.name > st || ep.args > st {
                return Err(bad("entrypoint string offset out of range"));
            }
        }
        if !self.entrypoints.is_empty()
            && self.header.default_entrypoint as usize >= self.entrypoints.len()
        {
            return Err(bad("default_entrypoint out of range"));
        }
        for &off in &self.lib_dir_offsets {
            if off > st {
                return Err(bad("lib_dir offset out of range"));
            }
        }
        if self.header.name_offset as u32 > st {
            return Err(bad("name_offset out of range"));
        }
        Ok(())
    }

    /// Returns the package name, or empty string if unset.
    pub fn name(&self) -> &str {
        if self.header.name_offset > 0 {
            self.get_string(self.header.name_offset as u32)
        } else {
            ""
        }
    }

    /// Returns resolved library directory paths.
    pub fn lib_dirs(&self) -> Vec<&str> {
        self.lib_dir_offsets
            .iter()
            .map(|&offset| self.get_string(offset))
            .collect()
    }

    pub fn get_string(&self, offset: u32) -> &str {
        let start = offset as usize;
        let Some(tail) = self.string_table.get(start..) else {
            return "";
        };
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        std::str::from_utf8(&tail[..end]).unwrap_or("")
    }

    /// Check if a top-level directory with the given name exists
    pub fn has_toplevel_dir(&self, name: &str) -> bool {
        use crate::entry::EntryKind;
        self.entries.iter().any(|e| {
            e.kind == EntryKind::Dir && e.parent == u32::MAX && self.get_string(e.name) == name
        })
    }

    /// Find the path to a lib directory if one exists
    /// Returns the path (e.g., "lib" or "overlayed/lib") or empty string if not found
    pub fn find_lib_dir(&self) -> String {
        use crate::entry::EntryKind;
        for (i, e) in self.entries.iter().enumerate() {
            if e.kind == EntryKind::Dir && self.get_string(e.name) == "lib" {
                return self.entry_path(i);
            }
        }
        String::new()
    }

    /// Reconstruct the full path for an entry by walking parent chain.
    ///
    /// The walk is bounded to the number of entries, so a crafted parent
    /// cycle terminates instead of looping forever, and an out-of-range
    /// parent index stops the walk instead of panicking.
    pub fn entry_path(&self, index: usize) -> String {
        let mut parts = Vec::new();
        let mut idx = index;
        for _ in 0..=self.entries.len() {
            let Some(entry) = self.entries.get(idx) else {
                break;
            };
            let name = self.get_string(entry.name);
            if name.is_empty() {
                break;
            }
            parts.push(name);
            if entry.parent == u32::MAX {
                break;
            }
            idx = entry.parent as usize;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Like [`Manifest::entry_path`], but validates every component is a
    /// safe single path segment (no `..`, no absolute or embedded `/`,
    /// no NUL) and returns a relative [`PathBuf`]. Callers extracting to
    /// disk MUST use this and confirm the joined path stays under the
    /// target root. Errors on an unsafe component or out-of-range parent.
    pub fn validated_entry_path(&self, index: usize) -> io::Result<PathBuf> {
        let mut parts: Vec<&str> = Vec::new();
        let mut idx = index;
        for _ in 0..=self.entries.len() {
            let entry = self.entries.get(idx).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "entry parent index out of range",
                )
            })?;
            let name = self.get_string(entry.name);
            if name.is_empty() {
                break;
            }
            if !is_safe_component(name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsafe path component: {name:?}"),
                ));
            }
            parts.push(name);
            if entry.parent == u32::MAX {
                break;
            }
            idx = entry.parent as usize;
        }
        parts.reverse();
        let mut path = PathBuf::new();
        for part in parts {
            path.push(part);
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Block, Entry, EntryKind};

    fn entry(name: u32, parent: u32) -> Entry {
        Entry {
            kind: EntryKind::Dir,
            parent,
            name,
            mode: 0o755,
            mtime_secs: 0,
            mtime_nsec: 0,
            content_hash: [0u8; 32],
            blocks: Vec::new(),
            symlink_target: 0,
        }
    }

    fn manifest(entries: Vec<Entry>, string_table: Vec<u8>) -> Manifest {
        Manifest {
            header: ManifestHeader {
                version: 1,
                entry_count: entries.len() as u32,
                string_table_size: string_table.len() as u32,
                entrypoint_count: 0,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [0u8; 32],
            },
            entrypoints: Vec::new(),
            entries,
            lib_dir_offsets: Vec::new(),
            string_table,
        }
    }

    #[test]
    fn roundtrip_valid_manifest() {
        // string table: "\0a\0b\0" -> "a" at 1, "b" at 3
        let st = b"\0a\0b\0".to_vec();
        let m = manifest(vec![entry(1, u32::MAX), entry(3, 0)], st);
        let bytes = m.serialize().unwrap();
        let back = Manifest::deserialize(&bytes).unwrap();
        assert_eq!(back.entry_path(1), "a/b");
        assert_eq!(back.get_string(3), "b");
    }

    #[test]
    fn file_entry_with_blocks_roundtrips_byte_identical() {
        // A File entry carrying two payload blocks. The serialized block
        // count is derived from `blocks.len()` now that `num_blocks` is gone;
        // this asserts the derivation and a byte-identical re-serialize.
        let mut file = entry(1, u32::MAX);
        file.kind = EntryKind::File;
        file.blocks = vec![
            Block {
                payload_offset: 0,
                compressed_size: 10,
                original_size: 20,
            },
            Block {
                payload_offset: 10,
                compressed_size: 5,
                original_size: 8,
            },
        ];
        let m = manifest(vec![file], b"\0a\0".to_vec());

        let bytes = m.serialize().unwrap();
        let back = Manifest::deserialize(&bytes).unwrap();
        assert_eq!(back.entries[0].blocks.len(), 2);
        assert_eq!(back.entries[0].blocks[1].original_size, 8);
        // Re-serializing the decoded manifest must reproduce the exact bytes.
        assert_eq!(back.serialize().unwrap(), bytes);
    }

    #[test]
    fn get_string_out_of_range_returns_empty() {
        let m = manifest(vec![entry(0, u32::MAX)], b"\0".to_vec());
        assert_eq!(m.get_string(9999), "");
    }

    #[test]
    fn oversized_entry_count_errors_without_panic() {
        let m = manifest(vec![entry(1, u32::MAX)], b"\0a\0".to_vec());
        let mut bytes = m.serialize().unwrap();
        // entry_count lives at header offset 2..6.
        bytes[2..6].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Manifest::deserialize(&bytes).is_err());
    }

    #[test]
    fn oversized_string_table_errors() {
        let m = manifest(vec![entry(1, u32::MAX)], b"\0a\0".to_vec());
        let mut bytes = m.serialize().unwrap();
        // string_table_size lives at header offset 6..10.
        bytes[6..10].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Manifest::deserialize(&bytes).is_err());
    }

    #[test]
    fn out_of_range_parent_rejected_at_deserialize() {
        let m = manifest(vec![entry(1, 5)], b"\0a\0".to_vec());
        let bytes = m.serialize().unwrap();
        assert!(Manifest::deserialize(&bytes).is_err());
    }

    #[test]
    fn parent_cycle_terminates() {
        // entries 0 and 1 point at each other; entry_path must not hang.
        let st = b"\0a\0b\0".to_vec();
        let m = manifest(vec![entry(1, 1), entry(3, 0)], st);
        let path = m.entry_path(0);
        assert!(!path.is_empty());
    }

    #[test]
    fn unsafe_component_rejected() {
        // name ".." at offset 1.
        let m = manifest(vec![entry(1, u32::MAX)], b"\0..\0".to_vec());
        assert!(m.validated_entry_path(0).is_err());
    }

    #[test]
    fn safe_component_accepted() {
        let m = manifest(vec![entry(1, u32::MAX)], b"\0a\0".to_vec());
        assert_eq!(m.validated_entry_path(0).unwrap().to_str(), Some("a"));
    }

    #[test]
    fn is_safe_component_rules() {
        assert!(is_safe_component("lib"));
        assert!(!is_safe_component(""));
        assert!(!is_safe_component("."));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("a/b"));
        assert!(!is_safe_component("a\0b"));
    }

    #[test]
    fn symlink_within_root_rules() {
        use std::path::Path;
        // Relative target that stays inside the root.
        assert!(symlink_target_within_root(
            Path::new("bin/app"),
            "../lib/x.so"
        ));
        assert!(symlink_target_within_root(Path::new("a/b/c"), "d"));
        // Absolute targets are refused.
        assert!(!symlink_target_within_root(
            Path::new("bin/app"),
            "/etc/passwd"
        ));
        // Walking above the root is refused.
        assert!(!symlink_target_within_root(
            Path::new("bin/app"),
            "../../etc/passwd"
        ));
        assert!(!symlink_target_within_root(Path::new("top"), "../escape"));
        // Empty target is refused.
        assert!(!symlink_target_within_root(Path::new("x"), ""));
    }
}

/// Helper for building a string table during packing.
#[derive(Debug, Default)]
pub struct StringTableBuilder {
    data: Vec<u8>,
    index: HashMap<String, u32>,
}

impl StringTableBuilder {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Add a string and return its offset in the table.
    pub fn add(&mut self, s: &str) -> u32 {
        if let Some(&offset) = self.index.get(s) {
            return offset;
        }
        let offset = self.data.len() as u32;
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0);
        self.index.insert(s.to_owned(), offset);
        offset
    }

    pub fn finish(self) -> Vec<u8> {
        self.data
    }

    /// Current size of the table in bytes, which is also the offset the
    /// next distinct string will be added at.
    pub fn len(&self) -> u32 {
        self.data.len() as u32
    }

    /// True before any string has been added. A table that has had even
    /// the empty string added is non-empty, since each entry contributes
    /// its terminating NUL.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}
