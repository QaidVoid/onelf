use std::collections::HashMap;
use std::io::{self, Cursor, Read, Write};

use crate::entry::{Entry, EntryPoint};

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
        let mut cursor = Cursor::new(data);
        let header = ManifestHeader::read_from(&mut cursor)?;

        if header.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported manifest version: {}", header.version),
            ));
        }

        let mut entrypoints = Vec::with_capacity(header.entrypoint_count as usize);
        for _ in 0..header.entrypoint_count {
            entrypoints.push(EntryPoint::read_from(&mut cursor)?);
        }

        let mut entries = Vec::with_capacity(header.entry_count as usize);
        for _ in 0..header.entry_count {
            entries.push(Entry::read_from(&mut cursor)?);
        }

        let mut lib_dir_offsets = Vec::with_capacity(header.lib_dir_count as usize);
        for _ in 0..header.lib_dir_count {
            let mut offset_buf = [0u8; 4];
            cursor.read_exact(&mut offset_buf)?;
            lib_dir_offsets.push(u32::from_le_bytes(offset_buf));
        }

        let mut string_table = vec![0u8; header.string_table_size as usize];
        cursor.read_exact(&mut string_table)?;

        Ok(Manifest {
            header,
            entrypoints,
            entries,
            lib_dir_offsets,
            string_table,
        })
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
        let end = self.string_table[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| start + p)
            .unwrap_or(self.string_table.len());
        std::str::from_utf8(&self.string_table[start..end]).unwrap_or("")
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

    /// Reconstruct the full path for an entry by walking parent chain
    pub fn entry_path(&self, index: usize) -> String {
        let mut parts = Vec::new();
        let mut idx = index;
        loop {
            let entry = &self.entries[idx];
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

    pub fn len(&self) -> u32 {
        self.data.len() as u32
    }
}
