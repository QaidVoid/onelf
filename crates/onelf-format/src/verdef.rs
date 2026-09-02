//! Dynamic-section facts of a shared object: the versions it defines and
//! the sonames it needs.
//!
//! A versioned library lists the versions it defines in its
//! `SHT_GNU_verdef` section. Two copies of the same soname can be ordered
//! by those sets: under symbol versioning a newer build only ever adds
//! versions, so the copy whose set contains the other's can stand in for
//! it. That ordering is what lets a host library and a bundled one be
//! compared without running either.
//!
//! Only the sections involved are read, so a large library costs a few
//! small reads rather than a whole-file parse. Every offset and count is
//! checked against the file before it is used, since the host's libraries
//! are not the package's to trust.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// The versions a shared object defines, excluding the base version that
/// only names the object itself.
pub type VersionSet = BTreeSet<String>;

/// Which copy of a soname the version comparison chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// The host's copy defines every version the bundled one does, and
    /// more.
    Host,
    /// The bundled copy defines everything the host's does.
    Bundle,
    /// Each copy defines a version the other lacks. Reported so the
    /// publisher can see it; the bundled copy is used.
    Incomparable,
}

/// Order two version sets. Equal sets and two empty sets choose the
/// bundle, since nothing about the host copy is known to be better.
pub fn compare(bundle: &VersionSet, host: &VersionSet) -> Choice {
    if host.is_subset(bundle) {
        Choice::Bundle
    } else if bundle.is_subset(host) {
        Choice::Host
    } else {
        Choice::Incomparable
    }
}

const SHT_DYNAMIC: u32 = 6;
const SHT_GNU_VERDEF: u32 = 0x6fff_fffd;
const VER_FLG_BASE: u16 = 0x1;
const VERDEF_ENTRY: usize = 20;
const VERDAUX_ENTRY: usize = 8;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;

/// The section header table is bounded so a corrupt count cannot drive a
/// large allocation; a real table is a few kilobytes.
const MAX_SHDR_TABLE: usize = 1 << 20;
/// A version definition or dynamic section is a few hundred bytes; a
/// string table can be large in a library with many symbols.
const MAX_SECTION: usize = 1 << 20;
const MAX_STRTAB: usize = 64 << 20;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Read the version definitions of the shared object at `path`.
///
/// An object without a version definition section yields an empty set. A
/// file that is not an ELF object, or whose tables point outside the file,
/// is an error rather than an empty set, so the caller can tell "defines
/// nothing" from "cannot be read".
pub fn read(path: &Path) -> io::Result<VersionSet> {
    let mut object = Object::open(path)?;
    let Some((defs, strings)) = object.section_with_strings(SHT_GNU_VERDEF)? else {
        return Ok(VersionSet::new());
    };
    parse_verdef(&defs, &strings)
}

/// Read the sonames the shared object at `path` needs, in table order.
/// An object without a dynamic section needs nothing.
pub fn read_needed(path: &Path) -> io::Result<Vec<String>> {
    let mut object = Object::open(path)?;
    let Some((dynamic, strings)) = object.section_with_strings(SHT_DYNAMIC)? else {
        return Ok(Vec::new());
    };
    parse_needed(&dynamic, &strings, object.layout.class64)
}

/// An opened ELF object with its section header table loaded.
struct Object {
    file: File,
    file_len: u64,
    layout: Layout,
    table: Vec<u8>,
}

impl Object {
    fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut header = [0u8; 64];
        let n = file.read(&mut header)?;
        let layout = Layout::parse(&header[..n])?;

        let table_len = layout
            .shnum
            .checked_mul(layout.shentsize)
            .ok_or_else(|| invalid("section header table overflows"))?;
        if table_len > MAX_SHDR_TABLE {
            return Err(invalid("section header table is implausibly large"));
        }
        let table = read_at(&mut file, file_len, layout.shoff, table_len)?;
        Ok(Object {
            file,
            file_len,
            layout,
            table,
        })
    }

    /// The first section of type `kind` and the string table it links to,
    /// or `None` when the object has no such section.
    fn section_with_strings(&mut self, kind: u32) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(section) = self.layout.find_section(&self.table, kind)? else {
            return Ok(None);
        };
        if section.size > MAX_SECTION {
            return Err(invalid("section is implausibly large"));
        }
        let strtab = self.layout.section(&self.table, section.link)?;
        if strtab.size > MAX_STRTAB {
            return Err(invalid("string table is implausibly large"));
        }
        let body = read_at(&mut self.file, self.file_len, section.offset, section.size)?;
        let strings = read_at(&mut self.file, self.file_len, strtab.offset, strtab.size)?;
        Ok(Some((body, strings)))
    }
}

/// Read `len` bytes at `offset`, refusing before the read if the range does
/// not fit the file.
fn read_at(file: &mut File, file_len: u64, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let end = offset
        .checked_add(len as u64)
        .ok_or_else(|| invalid("section range overflows"))?;
    if end > file_len {
        return Err(invalid("section lies outside the file"));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

struct Section {
    offset: u64,
    size: usize,
    link: usize,
}

/// Where the section header table sits and how its entries are laid out,
/// for the ELF class of the file.
struct Layout {
    class64: bool,
    shoff: u64,
    shentsize: usize,
    shnum: usize,
}

impl Layout {
    fn parse(header: &[u8]) -> io::Result<Self> {
        if header.len() < 64 || header[0..4] != *b"\x7fELF" {
            return Err(invalid("not an ELF object"));
        }
        if header[5] != 1 {
            return Err(invalid("big-endian ELF is not supported"));
        }
        let layout = match header[4] {
            2 => Layout {
                class64: true,
                shoff: u64::from_le_bytes(header[40..48].try_into().unwrap()),
                shentsize: u16::from_le_bytes(header[58..60].try_into().unwrap()) as usize,
                shnum: u16::from_le_bytes(header[60..62].try_into().unwrap()) as usize,
            },
            1 => Layout {
                class64: false,
                shoff: u32::from_le_bytes(header[32..36].try_into().unwrap()) as u64,
                shentsize: u16::from_le_bytes(header[46..48].try_into().unwrap()) as usize,
                shnum: u16::from_le_bytes(header[48..50].try_into().unwrap()) as usize,
            },
            _ => return Err(invalid("unknown ELF class")),
        };
        let min_entry = if layout.class64 { 64 } else { 40 };
        if layout.shnum > 0 && layout.shentsize < min_entry {
            return Err(invalid("section header entries are too small"));
        }
        Ok(layout)
    }

    fn section(&self, table: &[u8], index: usize) -> io::Result<Section> {
        let start = index
            .checked_mul(self.shentsize)
            .ok_or_else(|| invalid("section index overflows"))?;
        let entry = table
            .get(start..start + self.shentsize)
            .ok_or_else(|| invalid("section index outside the table"))?;
        let (offset, size, link) = if self.class64 {
            (
                u64::from_le_bytes(entry[24..32].try_into().unwrap()),
                u64::from_le_bytes(entry[32..40].try_into().unwrap()),
                u32::from_le_bytes(entry[40..44].try_into().unwrap()),
            )
        } else {
            (
                u32::from_le_bytes(entry[16..20].try_into().unwrap()) as u64,
                u32::from_le_bytes(entry[20..24].try_into().unwrap()) as u64,
                u32::from_le_bytes(entry[24..28].try_into().unwrap()),
            )
        };
        let size = usize::try_from(size).map_err(|_| invalid("section size overflows"))?;
        Ok(Section {
            offset,
            size,
            link: link as usize,
        })
    }

    fn find_section(&self, table: &[u8], kind: u32) -> io::Result<Option<Section>> {
        for i in 0..self.shnum {
            let start = i * self.shentsize;
            let entry = &table[start..start + self.shentsize];
            let sh_type = u32::from_le_bytes(entry[4..8].try_into().unwrap());
            if sh_type == kind {
                return self.section(table, i).map(Some);
            }
        }
        Ok(None)
    }
}

/// Walk the `Elf_Verdef` chain in `defs`, taking each definition's first
/// `Elf_Verdaux` as its name.
fn parse_verdef(defs: &[u8], strings: &[u8]) -> io::Result<VersionSet> {
    let mut out = VersionSet::new();
    if defs.is_empty() {
        return Ok(out);
    }
    let mut pos = 0usize;
    // A chain cannot legitimately have more links than fit in the section;
    // a corrupt one that never reaches zero is cut here.
    for _ in 0..=defs.len() / VERDEF_ENTRY {
        let entry = defs
            .get(pos..pos + VERDEF_ENTRY)
            .ok_or_else(|| invalid("version definition outside its section"))?;
        let flags = u16::from_le_bytes(entry[2..4].try_into().unwrap());
        let aux = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        let next = u32::from_le_bytes(entry[16..20].try_into().unwrap()) as usize;

        if flags & VER_FLG_BASE == 0 {
            let aux_at = pos
                .checked_add(aux)
                .ok_or_else(|| invalid("version auxiliary offset overflows"))?;
            let vda = defs
                .get(aux_at..aux_at + VERDAUX_ENTRY)
                .ok_or_else(|| invalid("version name entry outside its section"))?;
            let name_at = u32::from_le_bytes(vda[0..4].try_into().unwrap()) as usize;
            out.insert(string_at(strings, name_at)?);
        }

        if next == 0 {
            return Ok(out);
        }
        pos = pos
            .checked_add(next)
            .ok_or_else(|| invalid("version definition chain overflows"))?;
    }
    Err(invalid("version definition chain does not terminate"))
}

/// Collect the `DT_NEEDED` entries of a dynamic section.
fn parse_needed(dynamic: &[u8], strings: &[u8], class64: bool) -> io::Result<Vec<String>> {
    let entry_len = if class64 { 16 } else { 8 };
    let mut out = Vec::new();
    for entry in dynamic.chunks_exact(entry_len) {
        let (tag, value) = if class64 {
            (
                u64::from_le_bytes(entry[0..8].try_into().unwrap()),
                u64::from_le_bytes(entry[8..16].try_into().unwrap()),
            )
        } else {
            (
                u32::from_le_bytes(entry[0..4].try_into().unwrap()) as u64,
                u32::from_le_bytes(entry[4..8].try_into().unwrap()) as u64,
            )
        };
        match tag {
            DT_NULL => break,
            DT_NEEDED => {
                let at = usize::try_from(value).map_err(|_| invalid("needed name overflows"))?;
                out.push(string_at(strings, at)?);
            }
            _ => {}
        }
    }
    Ok(out)
}

fn string_at(strings: &[u8], at: usize) -> io::Result<String> {
    let rest = strings
        .get(at..)
        .ok_or_else(|| invalid("name outside the string table"))?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| invalid("name is not terminated"))?;
    std::str::from_utf8(&rest[..end])
        .map(String::from)
        .map_err(|_| invalid("name is not UTF-8"))
}

/// Builders for synthetic shared objects, shared with the resolver's tests.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    /// A 64-bit ELF with a string table, a version definition section built
    /// from `defs` (each `(flags, name)`), and a dynamic section naming
    /// `needed`. Either list may be empty, in which case that section is
    /// still emitted but holds nothing.
    pub(crate) fn shared_object(defs: &[(u16, &str)], needed: &[&str]) -> Vec<u8> {
        shared_object_with_interp(defs, needed, None)
    }

    /// [`shared_object`] with a `PT_INTERP` program header naming `interp`,
    /// the way a libc names the loader it belongs with.
    pub(crate) fn shared_object_with_interp(
        defs: &[(u16, &str)],
        needed: &[&str],
        interp: Option<&str>,
    ) -> Vec<u8> {
        let mut strings: Vec<u8> = vec![0];
        let mut intern = |s: &str| {
            let at = strings.len() as u32;
            strings.extend_from_slice(s.as_bytes());
            strings.push(0);
            at
        };
        let name_offsets: Vec<u32> = defs.iter().map(|(_, n)| intern(n)).collect();
        let needed_offsets: Vec<u32> = needed.iter().map(|n| intern(n)).collect();

        let mut verdef = Vec::new();
        for (i, (flags, _)) in defs.iter().enumerate() {
            let last = i + 1 == defs.len();
            verdef.extend_from_slice(&1u16.to_le_bytes());
            verdef.extend_from_slice(&flags.to_le_bytes());
            verdef.extend_from_slice(&(i as u16).to_le_bytes());
            verdef.extend_from_slice(&1u16.to_le_bytes());
            verdef.extend_from_slice(&0u32.to_le_bytes());
            verdef.extend_from_slice(&(VERDEF_ENTRY as u32).to_le_bytes());
            let next = if last {
                0
            } else {
                (VERDEF_ENTRY + VERDAUX_ENTRY) as u32
            };
            verdef.extend_from_slice(&next.to_le_bytes());
            verdef.extend_from_slice(&name_offsets[i].to_le_bytes());
            verdef.extend_from_slice(&0u32.to_le_bytes());
        }

        let mut dynamic = Vec::new();
        for off in needed_offsets {
            dynamic.extend_from_slice(&DT_NEEDED.to_le_bytes());
            dynamic.extend_from_slice(&(off as u64).to_le_bytes());
        }
        dynamic.extend_from_slice(&DT_NULL.to_le_bytes());
        dynamic.extend_from_slice(&0u64.to_le_bytes());

        let mut elf = build_elf(&strings, &verdef, &dynamic);
        if let Some(interp) = interp {
            // One program header, placed after the section headers and
            // before the section bodies is not possible without moving
            // them, so it goes at the end with the string it names.
            let phoff = elf.len();
            let interp_at = phoff + 56;
            elf.resize(interp_at + interp.len() + 1, 0);
            elf[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
            elf[54..56].copy_from_slice(&56u16.to_le_bytes());
            elf[56..58].copy_from_slice(&1u16.to_le_bytes());
            elf[phoff..phoff + 4].copy_from_slice(&3u32.to_le_bytes());
            elf[phoff + 8..phoff + 16].copy_from_slice(&(interp_at as u64).to_le_bytes());
            elf[phoff + 32..phoff + 40].copy_from_slice(&((interp.len() + 1) as u64).to_le_bytes());
            elf[interp_at..interp_at + interp.len()].copy_from_slice(interp.as_bytes());
        }
        elf
    }

    pub(crate) fn build_elf(strings: &[u8], verdef: &[u8], dynamic: &[u8]) -> Vec<u8> {
        let shoff = 64usize;
        let shentsize = 64usize;
        let shnum = 4usize;
        let strings_at = shoff + shentsize * shnum;
        let verdef_at = strings_at + strings.len();
        let dynamic_at = verdef_at + verdef.len();
        let mut v = vec![0u8; dynamic_at + dynamic.len()];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = 2;
        v[5] = 1;
        v[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        v[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
        v[58..60].copy_from_slice(&(shentsize as u16).to_le_bytes());
        v[60..62].copy_from_slice(&(shnum as u16).to_le_bytes());
        let mut shdr = |index: usize, sh_type: u32, offset: usize, size: usize, link: u32| {
            let at = shoff + index * shentsize;
            v[at + 4..at + 8].copy_from_slice(&sh_type.to_le_bytes());
            v[at + 24..at + 32].copy_from_slice(&(offset as u64).to_le_bytes());
            v[at + 32..at + 40].copy_from_slice(&(size as u64).to_le_bytes());
            v[at + 40..at + 44].copy_from_slice(&link.to_le_bytes());
        };
        shdr(1, 3, strings_at, strings.len(), 0);
        shdr(2, SHT_GNU_VERDEF, verdef_at, verdef.len(), 1);
        shdr(3, SHT_DYNAMIC, dynamic_at, dynamic.len(), 1);
        v[strings_at..strings_at + strings.len()].copy_from_slice(strings);
        v[verdef_at..verdef_at + verdef.len()].copy_from_slice(verdef);
        v[dynamic_at..dynamic_at + dynamic.len()].copy_from_slice(dynamic);
        v
    }

    /// A fresh directory under the system temp dir for one test.
    pub(crate) fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onelf-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{build_elf, shared_object, temp_dir};
    use super::*;

    fn set(names: &[&str]) -> VersionSet {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = temp_dir("verdef").join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn compare_orders_by_superset() {
        let old = set(&["GLIBC_2.2.5", "GLIBC_2.17"]);
        let new = set(&["GLIBC_2.2.5", "GLIBC_2.17", "GLIBC_2.34"]);
        assert_eq!(compare(&old, &new), Choice::Host);
        assert_eq!(compare(&new, &old), Choice::Bundle);
        assert_eq!(compare(&new, &new), Choice::Bundle);
        assert_eq!(compare(&set(&[]), &set(&[])), Choice::Bundle);
        // An unversioned host copy never displaces a versioned bundle.
        assert_eq!(compare(&old, &set(&[])), Choice::Bundle);
        // An unversioned bundle against a versioned host reads as the host
        // satisfying every version the bundle demands, which is none.
        assert_eq!(compare(&set(&[]), &old), Choice::Host);
        let fork = set(&["GLIBC_2.2.5", "VENDOR_1.0"]);
        assert_eq!(compare(&old, &fork), Choice::Incomparable);
    }

    #[test]
    fn reads_definitions_and_skips_the_base_version() {
        let elf = shared_object(
            &[
                (VER_FLG_BASE, "libfoo.so.1"),
                (0, "FOO_1.0"),
                (0, "FOO_1.2"),
            ],
            &[],
        );
        let path = write_temp("libfoo.so.1", &elf);
        assert_eq!(read(&path).unwrap(), set(&["FOO_1.0", "FOO_1.2"]));
    }

    #[test]
    fn reads_needed_sonames_in_order() {
        let elf = shared_object(&[], &["libc.so.6", "libm.so.6"]);
        let path = write_temp("libbar.so.1", &elf);
        assert_eq!(read_needed(&path).unwrap(), ["libc.so.6", "libm.so.6"]);
        assert!(read(&path).unwrap().is_empty());
    }

    #[test]
    fn no_sections_means_nothing_defined_or_needed() {
        let mut v = vec![0u8; 64];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = 2;
        v[5] = 1;
        let path = write_temp("plain.so", &v);
        assert!(read(&path).unwrap().is_empty());
        assert!(read_needed(&path).unwrap().is_empty());
    }

    #[test]
    fn not_an_elf_is_an_error() {
        let path = write_temp("text", b"#!/bin/sh\n");
        assert!(read(&path).is_err());
        assert!(read_needed(&path).is_err());
    }

    #[test]
    fn truncated_section_is_an_error_not_a_panic() {
        let elf = shared_object(&[(0, "FOO_1.0")], &["libc.so.6"]);
        let path = write_temp("cut.so", &elf[..elf.len() - 4]);
        assert!(read_needed(&path).is_err());
        for cut in 0..elf.len() {
            let p = write_temp("cut.so", &elf[..cut]);
            let _ = read(&p);
            let _ = read_needed(&p);
        }
    }

    #[test]
    fn name_past_the_string_table_is_an_error() {
        let strings = b"\0FOO_1.0\0";
        let mut verdef = Vec::new();
        verdef.extend_from_slice(&1u16.to_le_bytes());
        verdef.extend_from_slice(&0u16.to_le_bytes());
        verdef.extend_from_slice(&0u16.to_le_bytes());
        verdef.extend_from_slice(&1u16.to_le_bytes());
        verdef.extend_from_slice(&0u32.to_le_bytes());
        verdef.extend_from_slice(&(VERDEF_ENTRY as u32).to_le_bytes());
        verdef.extend_from_slice(&0u32.to_le_bytes());
        verdef.extend_from_slice(&9999u32.to_le_bytes());
        verdef.extend_from_slice(&0u32.to_le_bytes());
        let mut dynamic = Vec::new();
        dynamic.extend_from_slice(&DT_NEEDED.to_le_bytes());
        dynamic.extend_from_slice(&9999u64.to_le_bytes());
        let path = write_temp("bad.so", &build_elf(strings, &verdef, &dynamic));
        assert!(read(&path).is_err());
        assert!(read_needed(&path).is_err());
    }

    #[test]
    fn a_chain_that_leaves_the_section_is_an_error() {
        let mut elf = shared_object(&[(0, "FOO_1.0"), (0, "FOO_1.1")], &[]);
        // The dynamic section holds only its terminating entry.
        let dynamic_len = 16;
        let verdef_at = elf.len() - dynamic_len - 2 * (VERDEF_ENTRY + VERDAUX_ENTRY);
        elf[verdef_at + 16..verdef_at + 20].copy_from_slice(&1000u32.to_le_bytes());
        let path = write_temp("runaway.so", &elf);
        assert!(read(&path).is_err());

        // A chain ending early is fine: only the entries reached count.
        let mut elf = shared_object(&[(0, "FOO_1.0"), (0, "FOO_1.1")], &[]);
        elf[verdef_at + 16..verdef_at + 20].copy_from_slice(&0u32.to_le_bytes());
        let path = write_temp("short.so", &elf);
        assert_eq!(read(&path).unwrap(), set(&["FOO_1.0"]));
    }

    #[test]
    fn reads_the_host_libc_when_present() {
        let candidates = [
            "/lib64/libc.so.6",
            "/usr/lib64/libc.so.6",
            "/usr/lib/libc.so.6",
            "/lib/x86_64-linux-gnu/libc.so.6",
            "/lib/aarch64-linux-gnu/libc.so.6",
        ];
        let Some(libc) = candidates.iter().find(|p| Path::new(p).is_file()) else {
            return;
        };
        let set = read(Path::new(libc)).unwrap();
        assert!(set.iter().any(|v| v.starts_with("GLIBC_2.")), "{set:?}");
        assert!(!set.iter().any(|v| v == "libc.so.6"));
        let needed = read_needed(Path::new(libc)).unwrap();
        assert!(
            needed.iter().any(|n| n.starts_with("ld-linux")),
            "{needed:?}"
        );
    }
}
