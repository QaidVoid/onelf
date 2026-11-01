//! Bounded ELF metadata inspection shared by the packer and runtime.
//!
//! Version-set ordering is a compatibility heuristic, not proof of ABI
//! compatibility. Facts come from the inspected file, never the host platform.
//! Only the relevant sections and program headers are read. Little-endian
//! ELF32 and ELF64 are supported without a whole-file parser dependency.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Versions defined by an object, excluding its base object-name version.
pub type VersionSet = BTreeSet<String>;

/// Which copy of a soname the version comparison chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// The host defines every bundled version and additional versions.
    Host,
    /// The bundle defines every host version, including equal sets.
    Bundle,
    /// Each copy defines a version the other lacks. Use the bundle.
    Incomparable,
}

/// Order version sets. Equal sets, including empty sets, choose the bundle.
pub fn compare(bundle: &VersionSet, host: &VersionSet) -> Choice {
    if host.is_subset(bundle) {
        Choice::Bundle
    } else if bundle.is_subset(host) {
        Choice::Host
    } else {
        Choice::Incomparable
    }
}

/// ELF address and word size. This is separate from the machine identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfClass {
    /// ELFCLASS32.
    Elf32,
    /// ELFCLASS64.
    Elf64,
}

/// Libc-family evidence in actual dependency names and the interpreter.
///
/// This name-based heuristic is not an ABI guarantee. In particular, an
/// object with no libc dependency has unknown family, not universal compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibcFamily {
    /// No recognized family evidence.
    Unknown,
    /// A glibc dependency or loader name was found.
    Glibc,
    /// A musl dependency or loader name was found.
    Musl,
    /// Both families were named. This must not be treated as compatible.
    Conflicting,
}

/// A GNU version requirement associated with its `vn_file` dependency name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredVersion {
    /// The required version's string-table name.
    pub name: String,
    /// The original GNU `vna_flags`, including `VER_FLG_WEAK` (bit 1).
    pub flags: u16,
}

impl RequiredVersion {
    /// Whether the loader treats this version requirement as weak.
    /// Weak requirements must not be enforced like mandatory versions.
    pub fn is_weak(&self) -> bool {
        self.flags & 2 != 0
    }
}

/// Metadata from one inspected ELF file. Missing versioning or SONAME is valid.
///
/// The basename alone does not imply export into a loader namespace. A provider
/// index must classify its manifest path and verify any additional symlink aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfFacts {
    /// The ELF class, independent of the host architecture.
    pub class: ElfClass,
    /// The ELF `e_machine` value, such as 3, 62, or 183.
    pub machine: u16,
    /// `DT_SONAME`, if the object declares one.
    pub soname: Option<String>,
    /// Basename of the inspected path, without canonicalizing symlinks.
    pub inspected_basename: String,
    /// `DT_NEEDED` dependency names in dynamic-table order.
    pub needed: Vec<String>,
    /// Defined GNU versions, excluding the base object name.
    pub defined_versions: VersionSet,
    /// Required GNU versions keyed by their `vn_file` dependency name.
    pub required_versions: BTreeMap<String, Vec<RequiredVersion>>,
    /// The actual `PT_INTERP` string, if present.
    pub interpreter: Option<String>,
    /// Family inferred only from dependency and interpreter names.
    pub libc_family: LibcFamily,
}

impl ElfFacts {
    /// Known loader-name candidates: declared SONAME and inspected basename.
    /// This does not discover filesystem aliases or establish namespace membership.
    pub fn known_loader_names(&self) -> BTreeSet<&str> {
        self.soname
            .as_deref()
            .into_iter()
            .chain(std::iter::once(self.inspected_basename.as_str()))
            .collect()
    }
}

const SHT_STRTAB: u32 = 3;
const SHT_DYNAMIC: u32 = 6;
const SHT_GNU_VERDEF: u32 = 0x6fff_fffd;
const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;
const VER_FLG_BASE: u16 = 1;
const VERDEF_ENTRY: usize = 20;
const VERDAUX_ENTRY: usize = 8;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_SONAME: u64 = 14;
const DT_VERNEED: u64 = 0x6fff_fffe;
const DT_VERNEEDNUM: u64 = 0x6fff_ffff;
const MAX_HEADER_TABLE: usize = 1 << 20;
const MAX_SECTION: usize = 1 << 20;
const MAX_STRTAB: usize = 64 << 20;
const MAX_NAME: usize = 4096;
const MAX_RECORDS: usize = 4096;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Read GNU version definitions. An unversioned object yields an empty set.
/// Malformed metadata is an error, never evidence of compatibility.
pub fn read(path: &Path) -> io::Result<VersionSet> {
    let mut object = Object::open(path)?;
    let Some((defs, strings)) = object.section_with_strings(SHT_GNU_VERDEF)? else {
        return Ok(VersionSet::new());
    };
    parse_verdef(&defs, &strings)
}

/// Read dependency names in dynamic-table order. A static object needs nothing.
/// Sectionless dynamic objects are unsupported and return `InvalidData`.
pub fn read_needed(path: &Path) -> io::Result<Vec<String>> {
    let mut object = Object::open(path)?;
    object.check_dynamic_coverage()?;
    let Some((dynamic, strings)) = object.section_with_strings(SHT_DYNAMIC)? else {
        return Ok(Vec::new());
    };
    Ok(parse_dynamic(&dynamic, &strings, object.layout.class64)?.needed)
}

/// Read bounded ELF facts without consulting the host or executing the object.
///
/// Relevant sections are limited to 1 MiB, string tables to 64 MiB, tables to
/// 4096 decoded records and names to 4096 bytes. Header tables are each at most
/// 1 MiB. Unsupported endian or extended numbering, sectionless `PT_DYNAMIC`,
/// malformed links, truncated chains and out-of-file ranges return `InvalidData`.
/// A sectionless static object is supported. No parse failure becomes empty facts.
pub fn read_facts(path: &Path) -> io::Result<ElfFacts> {
    let mut object = Object::open(path)?;
    object.check_dynamic_coverage()?;
    let dynamic = match object.section_with_strings(SHT_DYNAMIC)? {
        Some((bytes, strings)) => parse_dynamic(&bytes, &strings, object.layout.class64)?,
        None => Dynamic::default(),
    };
    let defined_versions = match object.section_with_strings(SHT_GNU_VERDEF)? {
        Some((bytes, strings)) => parse_verdef(&bytes, &strings)?,
        None => VersionSet::new(),
    };
    let requirements = object.section_with_strings(SHT_GNU_VERNEED)?;
    if dynamic.verneed.is_some() && requirements.is_none() {
        return Err(invalid("dynamic version requirements have no section"));
    }
    let required_versions = match requirements {
        Some((bytes, strings)) => {
            let (versions, count) = parse_verneed(&bytes, &strings, &dynamic.needed)?;
            if dynamic.verneed_num.is_some_and(|n| n != count as u64) {
                return Err(invalid(
                    "dynamic version requirement count disagrees with section",
                ));
            }
            let section = object
                .layout
                .find_section(&object.table, SHT_GNU_VERNEED)?
                .unwrap();
            if dynamic
                .verneed
                .is_some_and(|address| address != section.address)
            {
                return Err(invalid(
                    "dynamic version requirement address disagrees with section",
                ));
            }
            if section.info != count {
                return Err(invalid(
                    "version requirement section count disagrees with chain",
                ));
            }
            versions
        }
        None => {
            if dynamic.verneed_num.is_some_and(|n| n != 0) {
                return Err(invalid("version requirement count has no section"));
            }
            BTreeMap::new()
        }
    };
    let interpreter = object.interpreter()?;
    let libc_family = infer_libc(&dynamic.needed, interpreter.as_deref());
    let inspected_basename = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("inspected basename is not UTF-8"))?;
    if inspected_basename.len() > MAX_NAME {
        return Err(invalid("inspected basename is too long"));
    }
    Ok(ElfFacts {
        class: if object.layout.class64 {
            ElfClass::Elf64
        } else {
            ElfClass::Elf32
        },
        machine: object.layout.machine,
        soname: dynamic.soname,
        inspected_basename: inspected_basename.to_owned(),
        needed: dynamic.needed,
        defined_versions,
        required_versions,
        interpreter,
        libc_family,
    })
}

fn infer_libc(needed: &[String], interpreter: Option<&str>) -> LibcFamily {
    let mut glibc = false;
    let mut musl = false;
    for name in needed.iter().map(String::as_str).chain(interpreter) {
        let name = match name.rsplit_once('/') {
            Some((_, base)) => base,
            None => name,
        };
        glibc |= name == "libc.so.6"
            || name.starts_with("ld-linux")
            || name == "ld64.so.1"
            || name == "ld64.so.2";
        musl |= name.starts_with("libc.musl-") || name.starts_with("ld-musl-");
    }
    match (glibc, musl) {
        (true, false) => LibcFamily::Glibc,
        (false, true) => LibcFamily::Musl,
        (true, true) => LibcFamily::Conflicting,
        (false, false) => LibcFamily::Unknown,
    }
}

struct Object {
    file: File,
    file_len: u64,
    layout: Layout,
    table: Vec<u8>,
    programs: Vec<u8>,
}

impl Object {
    fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let header = read_at(&mut file, file_len, 0, file_len.min(64) as usize)?;
        let layout = Layout::parse(&header)?;
        let table_len = bounded_table(layout.shnum, layout.shentsize)?;
        let table = read_at(&mut file, file_len, layout.shoff, table_len)?;
        let program_len = bounded_table(layout.phnum, layout.phentsize)?;
        let programs = read_at(&mut file, file_len, layout.phoff, program_len)?;
        Ok(Self {
            file,
            file_len,
            layout,
            table,
            programs,
        })
    }

    fn section_with_strings(&mut self, kind: u32) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(section) = self.layout.find_section(&self.table, kind)? else {
            return Ok(None);
        };
        if section.size > MAX_SECTION {
            return Err(invalid("section is implausibly large"));
        }
        let strtab = self.layout.section(&self.table, section.link)?;
        if strtab.kind != SHT_STRTAB {
            return Err(invalid("linked section is not a string table"));
        }
        if strtab.size > MAX_STRTAB {
            return Err(invalid("string table is implausibly large"));
        }
        let body = read_at(&mut self.file, self.file_len, section.offset, section.size)?;
        let strings = read_at(&mut self.file, self.file_len, strtab.offset, strtab.size)?;
        Ok(Some((body, strings)))
    }

    fn check_dynamic_coverage(&self) -> io::Result<()> {
        let mut found = false;
        for entry in self.programs.chunks_exact(self.layout.phentsize.max(1)) {
            if u32_at(entry, 0) != 2 {
                continue;
            }
            if found {
                return Err(invalid("multiple dynamic program headers"));
            }
            found = true;
            let section = self
                .layout
                .find_section(&self.table, SHT_DYNAMIC)?
                .ok_or_else(|| invalid("PT_DYNAMIC without a dynamic section is unsupported"))?;
            let (offset, size) = if self.layout.class64 {
                (u64_at(entry, 8), u64_at(entry, 32))
            } else {
                (u32_at(entry, 4) as u64, u32_at(entry, 16) as u64)
            };
            if offset != section.offset || size != section.size as u64 {
                return Err(invalid("dynamic program header disagrees with section"));
            }
        }
        Ok(())
    }

    fn interpreter(&mut self) -> io::Result<Option<String>> {
        let mut out = None;
        for entry in self.programs.chunks_exact(self.layout.phentsize.max(1)) {
            if u32_at(entry, 0) != 3 {
                continue;
            }
            if out.is_some() {
                return Err(invalid("multiple ELF interpreters"));
            }
            let (offset, size) = if self.layout.class64 {
                (u64_at(entry, 8), u64_at(entry, 32))
            } else {
                (u32_at(entry, 4) as u64, u32_at(entry, 16) as u64)
            };
            if size == 0 || size > MAX_NAME as u64 {
                return Err(invalid("invalid interpreter length"));
            }
            let bytes = read_at(&mut self.file, self.file_len, offset, size as usize)?;
            if bytes.last() != Some(&0) || bytes[..bytes.len() - 1].contains(&0) {
                return Err(invalid("interpreter is not a terminated path"));
            }
            out = Some(string_at(&bytes, 0)?);
        }
        Ok(out)
    }
}

fn bounded_table(count: usize, stride: usize) -> io::Result<usize> {
    let len = count
        .checked_mul(stride)
        .ok_or_else(|| invalid("ELF table length overflows"))?;
    if len > MAX_HEADER_TABLE {
        return Err(invalid("ELF header table is implausibly large"));
    }
    Ok(len)
}

fn read_at(file: &mut File, file_len: u64, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let end = offset
        .checked_add(len as u64)
        .ok_or_else(|| invalid("ELF range overflows"))?;
    if end > file_len {
        return Err(invalid("ELF range lies outside the file"));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            invalid("ELF data was truncated while reading")
        } else {
            e
        }
    })?;
    Ok(buf)
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

struct Section {
    kind: u32,
    offset: u64,
    address: u64,
    size: usize,
    link: usize,
    info: usize,
}

struct Layout {
    class64: bool,
    machine: u16,
    shoff: u64,
    shentsize: usize,
    shnum: usize,
    phoff: u64,
    phentsize: usize,
    phnum: usize,
}

impl Layout {
    fn parse(header: &[u8]) -> io::Result<Self> {
        if header.len() < 16 || header[..4] != *b"\x7fELF" {
            return Err(invalid("not an ELF object"));
        }
        if header[5] != 1 {
            return Err(invalid("big-endian ELF is not supported"));
        }
        let class64 = match header[4] {
            1 => false,
            2 => true,
            _ => return Err(invalid("unknown ELF class")),
        };
        if header.len() < if class64 { 64 } else { 52 } {
            return Err(invalid("truncated ELF header"));
        }
        let layout = if class64 {
            Self {
                class64,
                machine: u16_at(header, 18),
                shoff: u64_at(header, 40),
                shentsize: u16_at(header, 58) as usize,
                shnum: u16_at(header, 60) as usize,
                phoff: u64_at(header, 32),
                phentsize: u16_at(header, 54) as usize,
                phnum: u16_at(header, 56) as usize,
            }
        } else {
            Self {
                class64,
                machine: u16_at(header, 18),
                shoff: u32_at(header, 32) as u64,
                shentsize: u16_at(header, 46) as usize,
                shnum: u16_at(header, 48) as usize,
                phoff: u32_at(header, 28) as u64,
                phentsize: u16_at(header, 42) as usize,
                phnum: u16_at(header, 44) as usize,
            }
        };
        if (layout.shnum == 0 && layout.shoff != 0) || layout.phnum == 0xffff {
            return Err(invalid("extended ELF table numbering is unsupported"));
        }
        if layout.shnum > 0
            && (layout.shoff == 0 || layout.shentsize < if class64 { 64 } else { 40 })
        {
            return Err(invalid("invalid section header layout"));
        }
        if layout.phnum > 0
            && (layout.phoff == 0 || layout.phentsize < if class64 { 56 } else { 32 })
        {
            return Err(invalid("invalid program header layout"));
        }
        Ok(layout)
    }

    fn section(&self, table: &[u8], index: usize) -> io::Result<Section> {
        if index >= self.shnum {
            return Err(invalid("section index outside the table"));
        }
        let start = index
            .checked_mul(self.shentsize)
            .ok_or_else(|| invalid("section index overflows"))?;
        let end = start
            .checked_add(self.shentsize)
            .ok_or_else(|| invalid("section range overflows"))?;
        let entry = table
            .get(start..end)
            .ok_or_else(|| invalid("section outside the table"))?;
        let (offset, size, link, info) = if self.class64 {
            (
                u64_at(entry, 24),
                u64_at(entry, 32),
                u32_at(entry, 40),
                u32_at(entry, 44),
            )
        } else {
            (
                u32_at(entry, 16) as u64,
                u32_at(entry, 20) as u64,
                u32_at(entry, 24),
                u32_at(entry, 28),
            )
        };
        Ok(Section {
            kind: u32_at(entry, 4),
            offset,
            address: if self.class64 {
                u64_at(entry, 16)
            } else {
                u32_at(entry, 12) as u64
            },
            size: usize::try_from(size).map_err(|_| invalid("section size overflows"))?,
            link: link as usize,
            info: info as usize,
        })
    }

    fn find_section(&self, table: &[u8], kind: u32) -> io::Result<Option<Section>> {
        let mut found = None;
        for i in 0..self.shnum {
            let section = self.section(table, i)?;
            if section.kind == kind {
                if found.is_some() {
                    return Err(invalid("duplicate ELF metadata section"));
                }
                found = Some(section);
            }
        }
        Ok(found)
    }
}

fn record(bytes: &[u8], pos: usize, len: usize) -> io::Result<&[u8]> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| invalid("ELF record range overflows"))?;
    bytes
        .get(pos..end)
        .ok_or_else(|| invalid("ELF record outside its section"))
}

fn advance(pos: usize, delta: usize, minimum: usize) -> io::Result<usize> {
    if delta < minimum {
        return Err(invalid("ELF chain overlaps or cycles"));
    }
    pos.checked_add(delta)
        .ok_or_else(|| invalid("ELF chain overflows"))
}

fn parse_verdef(defs: &[u8], strings: &[u8]) -> io::Result<VersionSet> {
    let mut out = VersionSet::new();
    if defs.is_empty() {
        return Ok(out);
    }
    let mut pos = 0;
    let mut total_aux = 0usize;
    for _ in 0..MAX_RECORDS {
        let entry = record(defs, pos, VERDEF_ENTRY)?;
        if u16_at(entry, 0) != 1 {
            return Err(invalid("unsupported version definition format"));
        }
        let count = u16_at(entry, 6) as usize;
        total_aux += count;
        if count == 0 || total_aux > MAX_RECORDS {
            return Err(invalid("invalid version definition auxiliary count"));
        }
        let mut aux_at = advance(pos, u32_at(entry, 12) as usize, VERDEF_ENTRY)?;
        for i in 0..count {
            let aux = record(defs, aux_at, VERDAUX_ENTRY)?;
            let name = string_at(strings, u32_at(aux, 0) as usize)?;
            if i == 0 && u16_at(entry, 2) & VER_FLG_BASE == 0 {
                out.insert(name);
            }
            let next = u32_at(aux, 4) as usize;
            if i + 1 == count {
                if next != 0 {
                    return Err(invalid(
                        "version definition auxiliary count disagrees with chain",
                    ));
                }
            } else {
                aux_at = advance(aux_at, next, VERDAUX_ENTRY)?;
            }
        }
        let next = u32_at(entry, 16) as usize;
        if next == 0 {
            return Ok(out);
        }
        pos = advance(pos, next, aux_at - pos + VERDAUX_ENTRY)?;
    }
    Err(invalid("too many version definitions"))
}

#[derive(Default)]
struct Dynamic {
    needed: Vec<String>,
    soname: Option<String>,
    verneed: Option<u64>,
    verneed_num: Option<u64>,
}

fn parse_dynamic(dynamic: &[u8], strings: &[u8], class64: bool) -> io::Result<Dynamic> {
    let stride = if class64 { 16 } else { 8 };
    if !dynamic.len().is_multiple_of(stride) {
        return Err(invalid("truncated dynamic entry"));
    }
    let mut out = Dynamic::default();
    for (i, entry) in dynamic.chunks_exact(stride).enumerate() {
        if i >= MAX_RECORDS {
            return Err(invalid("too many dynamic entries"));
        }
        let (tag, value) = if class64 {
            (u64_at(entry, 0), u64_at(entry, 8))
        } else {
            (u32_at(entry, 0) as u64, u32_at(entry, 4) as u64)
        };
        match tag {
            DT_NULL => return Ok(out),
            DT_NEEDED | DT_SONAME => {
                let name = string_at(
                    strings,
                    usize::try_from(value).map_err(|_| invalid("dynamic name offset overflows"))?,
                )?;
                if name.is_empty() {
                    return Err(invalid("empty dynamic loader name"));
                }
                if tag == DT_NEEDED {
                    out.needed.push(name);
                } else if out.soname.replace(name).is_some() {
                    return Err(invalid("duplicate SONAME"));
                }
            }
            DT_VERNEED if out.verneed.replace(value).is_some() => {
                return Err(invalid("duplicate version requirement address"));
            }
            DT_VERNEEDNUM if out.verneed_num.replace(value).is_some() => {
                return Err(invalid("duplicate version requirement count"));
            }
            _ => {}
        }
    }
    Err(invalid("dynamic section has no terminator"))
}

fn parse_verneed(
    bytes: &[u8],
    strings: &[u8],
    needed: &[String],
) -> io::Result<(BTreeMap<String, Vec<RequiredVersion>>, usize)> {
    let mut out = BTreeMap::new();
    if bytes.is_empty() {
        return Ok((out, 0));
    }
    let mut pos = 0;
    let mut total_aux = 0usize;
    for count in 1..=MAX_RECORDS {
        let entry = record(bytes, pos, 16)?;
        if u16_at(entry, 0) != 1 {
            return Err(invalid("unsupported version requirement format"));
        }
        let name = string_at(strings, u32_at(entry, 4) as usize)?;
        if !needed.contains(&name) {
            return Err(invalid(
                "version requirement does not name a needed dependency",
            ));
        }
        let aux_count = u16_at(entry, 2) as usize;
        total_aux = total_aux
            .checked_add(aux_count)
            .ok_or_else(|| invalid("version requirement count overflows"))?;
        if aux_count == 0 || total_aux > MAX_RECORDS {
            return Err(invalid("invalid version requirement auxiliary count"));
        }
        let versions = out.entry(name).or_insert_with(Vec::new);
        let mut aux_at = advance(pos, u32_at(entry, 8) as usize, 16)?;
        for i in 0..aux_count {
            let aux = record(bytes, aux_at, 16)?;
            let name = string_at(strings, u32_at(aux, 8) as usize)?;
            if name.is_empty() {
                return Err(invalid("empty required version name"));
            }
            versions.push(RequiredVersion {
                name,
                flags: u16_at(aux, 4),
            });
            let next = u32_at(aux, 12) as usize;
            if i + 1 == aux_count {
                if next != 0 {
                    return Err(invalid(
                        "version requirement auxiliary count disagrees with chain",
                    ));
                }
            } else {
                aux_at = advance(aux_at, next, 16)?;
            }
        }
        let next = u32_at(entry, 12) as usize;
        if next == 0 {
            return Ok((out, count));
        }
        pos = advance(pos, next, aux_at - pos + 16)?;
    }
    Err(invalid("too many version requirements"))
}

fn string_at(strings: &[u8], at: usize) -> io::Result<String> {
    let rest = strings
        .get(at..)
        .ok_or_else(|| invalid("name outside the string table"))?;
    let end = rest
        .iter()
        .take(MAX_NAME + 1)
        .position(|&b| b == 0)
        .ok_or_else(|| invalid("name is unterminated or exceeds 4096 bytes"))?;
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

#[cfg(test)]
mod facts_tests {
    use super::fixture::{shared_object, shared_object_with_interp, temp_dir};
    use super::*;

    struct Fixture {
        bytes: Vec<u8>,
        shoff: usize,
        shstride: usize,
        dynamic_at: usize,
        requirements_at: usize,
    }

    fn intern(strings: &mut Vec<u8>, name: &str) -> u32 {
        let offset = strings.len() as u32;
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
        offset
    }

    fn facts_elf(
        class64: bool,
        machine: u16,
        soname: Option<&str>,
        needed: &[&str],
        requirements: &[(&str, &[(u16, &str)])],
    ) -> Fixture {
        let mut strings = vec![0];
        let mut dynamic = Vec::new();
        let mut dyn_entry = |tag: u64, value: u64| {
            if class64 {
                dynamic.extend_from_slice(&tag.to_le_bytes());
                dynamic.extend_from_slice(&value.to_le_bytes());
            } else {
                dynamic.extend_from_slice(&(tag as u32).to_le_bytes());
                dynamic.extend_from_slice(&(value as u32).to_le_bytes());
            }
        };
        for name in needed {
            dyn_entry(DT_NEEDED, intern(&mut strings, name) as u64);
        }
        if let Some(name) = soname {
            dyn_entry(DT_SONAME, intern(&mut strings, name) as u64);
        }
        if !requirements.is_empty() {
            dyn_entry(DT_VERNEED, 0x5000);
            dyn_entry(DT_VERNEEDNUM, requirements.len() as u64);
        }
        dyn_entry(DT_NULL, 0);
        let mut verneed = Vec::new();
        for (i, (file, versions)) in requirements.iter().enumerate() {
            let start = verneed.len();
            let size = 16 + 16 * versions.len();
            verneed.resize(start + size, 0);
            verneed[start..start + 2].copy_from_slice(&1u16.to_le_bytes());
            verneed[start + 2..start + 4].copy_from_slice(&(versions.len() as u16).to_le_bytes());
            verneed[start + 4..start + 8]
                .copy_from_slice(&intern(&mut strings, file).to_le_bytes());
            verneed[start + 8..start + 12].copy_from_slice(&16u32.to_le_bytes());
            if i + 1 < requirements.len() {
                verneed[start + 12..start + 16].copy_from_slice(&(size as u32).to_le_bytes());
            }
            for (j, (flags, name)) in versions.iter().enumerate() {
                let at = start + 16 + j * 16;
                verneed[at + 4..at + 6].copy_from_slice(&flags.to_le_bytes());
                verneed[at + 6..at + 8].copy_from_slice(&((j + 2) as u16).to_le_bytes());
                verneed[at + 8..at + 12].copy_from_slice(&intern(&mut strings, name).to_le_bytes());
                if j + 1 < versions.len() {
                    verneed[at + 12..at + 16].copy_from_slice(&16u32.to_le_bytes());
                }
            }
        }
        let shoff = if class64 { 64 } else { 52 };
        let shstride = if class64 { 64 } else { 40 };
        let strings_at = shoff + 4 * shstride;
        let dynamic_at = strings_at + strings.len();
        let requirements_at = dynamic_at + dynamic.len();
        let mut bytes = vec![0; requirements_at + verneed.len()];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = if class64 { 2 } else { 1 };
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        if class64 {
            bytes[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
            bytes[58..60].copy_from_slice(&(shstride as u16).to_le_bytes());
            bytes[60..62].copy_from_slice(&4u16.to_le_bytes());
        } else {
            bytes[32..36].copy_from_slice(&(shoff as u32).to_le_bytes());
            bytes[46..48].copy_from_slice(&(shstride as u16).to_le_bytes());
            bytes[48..50].copy_from_slice(&4u16.to_le_bytes());
        }
        for (i, kind, offset, size, address, link, info) in [
            (1, SHT_STRTAB, strings_at, strings.len(), 0, 0, 0),
            (2, SHT_DYNAMIC, dynamic_at, dynamic.len(), 0, 1, 0),
            (
                3,
                SHT_GNU_VERNEED,
                requirements_at,
                verneed.len(),
                0x5000,
                1,
                requirements.len(),
            ),
        ] {
            let at = shoff + i * shstride;
            bytes[at + 4..at + 8].copy_from_slice(&kind.to_le_bytes());
            if class64 {
                bytes[at + 16..at + 24].copy_from_slice(&(address as u64).to_le_bytes());
                bytes[at + 24..at + 32].copy_from_slice(&(offset as u64).to_le_bytes());
                bytes[at + 32..at + 40].copy_from_slice(&(size as u64).to_le_bytes());
                bytes[at + 40..at + 44].copy_from_slice(&(link as u32).to_le_bytes());
                bytes[at + 44..at + 48].copy_from_slice(&(info as u32).to_le_bytes());
            } else {
                bytes[at + 12..at + 16].copy_from_slice(&(address as u32).to_le_bytes());
                bytes[at + 16..at + 20].copy_from_slice(&(offset as u32).to_le_bytes());
                bytes[at + 20..at + 24].copy_from_slice(&(size as u32).to_le_bytes());
                bytes[at + 24..at + 28].copy_from_slice(&(link as u32).to_le_bytes());
                bytes[at + 28..at + 32].copy_from_slice(&(info as u32).to_le_bytes());
            }
        }
        bytes[strings_at..dynamic_at].copy_from_slice(&strings);
        bytes[dynamic_at..requirements_at].copy_from_slice(&dynamic);
        bytes[requirements_at..].copy_from_slice(&verneed);
        Fixture {
            bytes,
            shoff,
            shstride,
            dynamic_at,
            requirements_at,
        }
    }

    fn inspect(bytes: &[u8]) -> io::Result<ElfFacts> {
        let dir = temp_dir("elf-facts");
        let path = dir.join("libalias.so");
        std::fs::write(&path, bytes).unwrap();
        let result = read_facts(&path);
        std::fs::remove_dir_all(dir).unwrap();
        result
    }

    fn assert_invalid(bytes: &[u8]) {
        assert_eq!(
            inspect(bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn class_machine_soname_and_known_alias_are_file_facts() {
        for (class64, machine, class) in [
            (false, 3, ElfClass::Elf32),
            (true, 62, ElfClass::Elf64),
            (true, 183, ElfClass::Elf64),
        ] {
            let fixture = facts_elf(class64, machine, Some("libreal.so.1"), &["libc.so.6"], &[]);
            let facts = inspect(&fixture.bytes).unwrap();
            assert_eq!(facts.class, class);
            assert_eq!(facts.machine, machine);
            assert_eq!(facts.soname.as_deref(), Some("libreal.so.1"));
            assert_eq!(
                facts.known_loader_names(),
                BTreeSet::from(["libalias.so", "libreal.so.1"])
            );
            assert_eq!(facts.libc_family, LibcFamily::Glibc);
            assert!(facts.required_versions.is_empty());
        }
    }

    #[test]
    fn requirements_keep_dependency_relationships_and_weak_flags() {
        for class64 in [false, true] {
            let fixture = facts_elf(
                class64,
                3,
                None,
                &["libc.so.6", "libfoo.so"],
                &[
                    ("libc.so.6", &[(0, "GLIBC_2.17"), (2, "GLIBC_2.34")]),
                    ("libfoo.so", &[(0x82, "FOO_OPTIONAL"), (0, "FOO_1")]),
                ],
            );
            let facts = inspect(&fixture.bytes).unwrap();
            assert_eq!(facts.needed, ["libc.so.6", "libfoo.so"]);
            assert_eq!(facts.required_versions.len(), 2);
            let libc = &facts.required_versions["libc.so.6"];
            assert_eq!(libc[0].name, "GLIBC_2.17");
            assert!(!libc[0].is_weak());
            assert_eq!(libc[1].name, "GLIBC_2.34");
            assert!(libc[1].is_weak());
            assert_eq!(facts.required_versions["libfoo.so"][0].flags, 0x82);
            assert!(facts.required_versions["libfoo.so"][0].is_weak());
            assert_eq!(facts.soname, None);
        }
    }

    #[test]
    fn unversioned_and_static_old_objects_are_not_host_classified() {
        let facts = inspect(&shared_object(&[], &[])).unwrap();
        assert!(facts.defined_versions.is_empty());
        assert!(facts.required_versions.is_empty());
        assert_eq!(facts.libc_family, LibcFamily::Unknown);
        for (length, class, expected) in [(52, 1, ElfClass::Elf32), (64, 2, ElfClass::Elf64)] {
            let mut bytes = vec![0; length];
            bytes[..4].copy_from_slice(b"\x7fELF");
            bytes[4] = class;
            bytes[5] = 1;
            let facts = inspect(&bytes).unwrap();
            assert_eq!(facts.class, expected);
            assert!(facts.needed.is_empty());
            assert_eq!(facts.libc_family, LibcFamily::Unknown);
        }
        let facts = inspect(&shared_object(&[(0, "FOO_1")], &[])).unwrap();
        assert_eq!(
            facts.defined_versions,
            BTreeSet::from(["FOO_1".to_string()])
        );
    }

    #[test]
    fn libc_family_uses_actual_dependencies_and_interpreter() {
        for (needed, interp, expected) in [
            (vec![], Some("/lib/ld-musl-x86_64.so.1"), LibcFamily::Musl),
            (vec!["libc.musl-aarch64.so.1"], None, LibcFamily::Musl),
            (
                vec![],
                Some("/lib64/ld-linux-x86-64.so.2"),
                LibcFamily::Glibc,
            ),
            (
                vec!["libc.so.6"],
                Some("/lib/ld-musl-i386.so.1"),
                LibcFamily::Conflicting,
            ),
            (
                vec!["libfoo.so"],
                Some("/custom/loader"),
                LibcFamily::Unknown,
            ),
        ] {
            let facts = inspect(&shared_object_with_interp(&[], &needed, interp)).unwrap();
            assert_eq!(facts.interpreter.as_deref(), interp);
            assert_eq!(facts.libc_family, expected);
        }
    }

    #[test]
    fn malformed_version_requirements_fail_instead_of_becoming_empty() {
        let fixture = facts_elf(
            true,
            62,
            None,
            &["libc.so.6"],
            &[("libc.so.6", &[(0, "GLIBC_2.17"), (2, "GLIBC_2.34")])],
        );
        let at = fixture.requirements_at;
        for (offset, value) in [
            (at + 4, u32::MAX),
            (at + 8, 0),
            (at + 8, u32::MAX),
            (at + 12, 1),
            (at + 12, u32::MAX),
            (at + 16 + 8, u32::MAX),
            (at + 16 + 12, 0),
            (at + 16 + 12, 1),
            (at + 32 + 12, 16),
        ] {
            let mut bytes = fixture.bytes.clone();
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert_invalid(&bytes);
        }
        let mut bytes = fixture.bytes.clone();
        bytes[at + 2..at + 4].copy_from_slice(&0xffffu16.to_le_bytes());
        assert_invalid(&bytes);
        let unrelated = facts_elf(
            true,
            62,
            None,
            &["libfoo.so"],
            &[("libc.so.6", &[(0, "GLIBC_2.17")])],
        );
        assert_invalid(&unrelated.bytes);
        for cut in 0..fixture.bytes.len() {
            assert_invalid(&fixture.bytes[..cut]);
        }
    }

    #[test]
    fn malformed_section_links_addresses_counts_and_dynamic_entries_fail() {
        let fixture = facts_elf(
            true,
            62,
            None,
            &["libc.so.6"],
            &[("libc.so.6", &[(0, "GLIBC_2.17")])],
        );
        let need_sh = fixture.shoff + 3 * fixture.shstride;
        for (offset, value) in [
            (need_sh + 40, 0),
            (need_sh + 40, 99),
            (need_sh + 44, 99),
            (fixture.shoff + fixture.shstride + 4, SHT_DYNAMIC),
            (need_sh + 4, 0),
        ] {
            let mut bytes = fixture.bytes.clone();
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert_invalid(&bytes);
        }
        for (offset, value) in [
            (need_sh + 16, 0x6000u64),
            (need_sh + 24, u64::MAX),
            (need_sh + 32, (MAX_SECTION + 1) as u64),
            (fixture.dynamic_at + 8, u64::MAX),
            (fixture.dynamic_at + 40, 99),
            (fixture.dynamic_at + 48, DT_NEEDED),
        ] {
            let mut bytes = fixture.bytes.clone();
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            assert_invalid(&bytes);
        }
        let mut bytes = fixture.bytes.clone();
        let dynamic_size = fixture.shoff + 2 * fixture.shstride + 32;
        bytes[dynamic_size..dynamic_size + 8].copy_from_slice(&63u64.to_le_bytes());
        assert_invalid(&bytes);
    }

    #[test]
    fn sectionless_dynamic_is_explicitly_unsupported_and_interp_is_bounded() {
        let mut bytes = shared_object_with_interp(&[], &[], Some("/lib/ld-musl-x86_64.so.1"));
        let phoff = u64_at(&bytes, 32) as usize;
        let interp_at = u64_at(&bytes[phoff..], 8) as usize;
        bytes[interp_at] = 0;
        assert_invalid(&bytes);
        bytes[phoff + 32..phoff + 40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_invalid(&bytes);
        bytes[40..48].copy_from_slice(&0u64.to_le_bytes());
        bytes[60..62].copy_from_slice(&0u16.to_le_bytes());
        bytes[phoff..phoff + 4].copy_from_slice(&2u32.to_le_bytes());
        let dir = temp_dir("sectionless-dynamic");
        let path = dir.join("dynamic.so");
        std::fs::write(&path, &bytes).unwrap();
        for error in [
            read_facts(&path).unwrap_err(),
            read_needed(&path).unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("unsupported"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_and_record_counts_are_bounded() {
        assert!(string_at(&vec![b'a'; MAX_NAME + 2], 0).is_err());
        let mut strings = vec![b'a'; MAX_NAME];
        strings.push(0);
        assert_eq!(string_at(&strings, 0).unwrap().len(), MAX_NAME);
        let mut dynamic = Vec::new();
        for _ in 0..MAX_RECORDS {
            dynamic.extend_from_slice(&[2u64.to_le_bytes(), 0u64.to_le_bytes()].concat());
        }
        dynamic.extend_from_slice(&[0; 16]);
        assert!(parse_dynamic(&dynamic, b"\0", true).is_err());
        assert!(bounded_table(usize::MAX, 64).is_err());
        assert!(record(&[], usize::MAX, 16).is_err());
    }

    #[test]
    fn real_consumer_smoke_without_a_compiler() {
        let path = Path::new("/proc/self/exe");
        if !path.exists() {
            return;
        }
        let facts = read_facts(path).unwrap();
        assert_eq!(
            facts.class,
            if usize::BITS == 64 {
                ElfClass::Elf64
            } else {
                ElfClass::Elf32
            }
        );
        assert!(facts.soname.is_none());
        for dependency in facts.required_versions.keys() {
            assert!(facts.needed.contains(dependency));
        }
    }
}
