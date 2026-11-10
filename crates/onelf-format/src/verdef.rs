//! Bounded ELF metadata inspection shared by the packer and runtime.
//!
//! Version-set ordering is a compatibility heuristic, not proof of ABI
//! compatibility. Facts come from the inspected file, never the host platform.
//! Facts are read from the dynamic table and the load segments that map it,
//! which are the bytes the loader itself reads; section headers never speak,
//! so a table that lies cannot change what is checked. Little-endian ELF32
//! and ELF64 are supported without a whole-file parser dependency.

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
    /// The object carries at least one `PT_LOAD` segment, so the loader
    /// has something to map. A file that is only an ELF header cannot be
    /// opened as a shared object at all.
    pub mappable: bool,
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
    /// An old-style `DT_RPATH` entry is present. A launch searches an
    /// rpath before its library path, so the copy a dependency loads
    /// cannot be modeled. Superseded when `has_runpath` is also set.
    pub has_rpath: bool,
    /// A `DT_RUNPATH` entry is present. A launch searches a runpath
    /// after its library path, so the modeled suppliers still win.
    pub has_runpath: bool,
    /// `DT_FILTER` or `DT_AUXILIARY` is present. A filter loads another
    /// object from outside the walked dependency closure.
    pub filtered: bool,
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

const VER_FLG_BASE: u16 = 1;
const VERDEF_ENTRY: usize = 20;
const VERDAUX_ENTRY: usize = 8;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_SONAME: u64 = 14;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;
const DT_FILTER: u64 = 0x7fff_ffff;
const DT_AUXILIARY: u64 = 0x7fff_fffd;
const DT_VERDEF: u64 = 0x6fff_fffc;
const DT_VERNEED: u64 = 0x6fff_fffe;
const DT_VERNEEDNUM: u64 = 0x6fff_ffff;
const MAX_HEADER_TABLE: usize = 1 << 20;
const MAX_TABLE: usize = 1 << 20;
const MAX_STRTAB: usize = 64 << 20;
const MAX_NAME: usize = 4096;
const MAX_RECORDS: usize = 4096;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// The System V ELF hash of `name`, the value the loader's version
/// matching compares `DT_VERDEF` and `DT_VERNEED` entries against.
pub fn elf_hash(name: &str) -> u32 {
    let mut hash: u32 = 0;
    for byte in name.bytes() {
        hash = hash.wrapping_shl(4).wrapping_add(u32::from(byte));
        let high = hash & 0xf000_0000;
        if high != 0 {
            hash ^= high >> 24;
        }
        hash &= !high;
    }
    hash
}

/// Read GNU version definitions. An unversioned object yields an empty set.
/// Malformed metadata is an error, never evidence of compatibility.
pub fn read(path: &Path) -> io::Result<VersionSet> {
    read_facts(path).map(|facts| facts.defined_versions)
}

/// Read dependency names in dynamic-table order. An object without a
/// dynamic table, such as a static one, needs nothing.
pub fn read_needed(path: &Path) -> io::Result<Vec<String>> {
    read_facts(path).map(|facts| facts.needed)
}

/// Read bounded ELF facts without consulting the host or executing the object.
///
/// Everything is read the way the loader reads it: from the program
/// headers, the `PT_DYNAMIC` segment through the load segments that map
/// it, and the version tables its tags address through those segments.
/// Load segments that leave the file, overlap, or share a page from
/// different file distances are refused, as are non-regular files: the
/// loader would not run them, or would map bytes the reader cannot
/// attribute. Section headers are never consulted, so a table that lies
/// about them cannot change the facts. Version entries must carry the
/// System V hash of their name, which the loader's match requires.
/// Tables are limited to 1 MiB, string tables to 64 MiB, decoded records
/// to 4096 and names to 4096 bytes. Header tables are each at most 1 MiB.
/// Unsupported endian or extended numbering, addresses outside the
/// mapped segments, malformed links, truncated chains and out-of-file
/// ranges return `InvalidData`. An object without a dynamic table, such
/// as a static one, holds no dynamic facts. No parse failure becomes
/// empty facts.
pub fn read_facts(path: &Path) -> io::Result<ElfFacts> {
    let mut object = Object::open(path)?;
    let loads = object.load_segments()?;
    let (needed, soname, has_rpath, has_runpath, filtered, defined_versions, required_versions) =
        match object.dynamic_segment()? {
            Some((vaddr, len)) => {
                let bytes = object.read_mapped(&loads, vaddr, len)?;
                let tags = parse_dynamic_table(&bytes, object.layout.class64)?;
                let strings: Vec<u8> = match tags.strtab {
                    Some(vaddr) => object.read_mapped(&loads, vaddr, MAX_STRTAB)?,
                    None => Vec::new(),
                };
                let mut needed = Vec::with_capacity(tags.needed.len());
                for offset in tags.needed {
                    let offset = usize::try_from(offset)
                        .map_err(|_| invalid("dynamic name offset overflows"))?;
                    let name = string_at(&strings, offset)?;
                    if name.is_empty() {
                        return Err(invalid("empty dynamic loader name"));
                    }
                    needed.push(name);
                }
                let soname = match tags.soname {
                    Some(offset) => {
                        let offset = usize::try_from(offset)
                            .map_err(|_| invalid("dynamic name offset overflows"))?;
                        let name = string_at(&strings, offset)?;
                        if name.is_empty() {
                            return Err(invalid("empty dynamic loader name"));
                        }
                        Some(name)
                    }
                    None => None,
                };
                let defined_versions = match tags.verdef {
                    Some(vaddr) => {
                        let defs = object.read_mapped(&loads, vaddr, MAX_TABLE)?;
                        parse_verdef(&defs, &strings)?.0
                    }
                    None => VersionSet::new(),
                };
                let required_versions = match tags.verneed {
                    Some(vaddr) => {
                        let bytes = object.read_mapped(&loads, vaddr, MAX_TABLE)?;
                        let (versions, count) = parse_verneed(&bytes, &strings, &needed)?;
                        if tags.verneed_num.is_some_and(|n| n != count as u64) {
                            return Err(invalid(
                                "dynamic version requirement count disagrees with chain",
                            ));
                        }
                        versions
                    }
                    None => {
                        if tags.verneed_num.is_some_and(|n| n != 0) {
                            return Err(invalid("version requirement count has no table"));
                        }
                        BTreeMap::new()
                    }
                };
                (
                    needed,
                    soname,
                    tags.has_rpath,
                    tags.has_runpath,
                    tags.filtered,
                    defined_versions,
                    required_versions,
                )
            }
            None => (
                Vec::new(),
                None,
                false,
                false,
                false,
                VersionSet::new(),
                BTreeMap::new(),
            ),
        };
    let interpreter = object.interpreter(&loads)?;
    let libc_family = infer_libc(&needed, interpreter.as_deref());
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
        mappable: !loads.is_empty(),
        machine: object.layout.machine,
        soname,
        inspected_basename: inspected_basename.to_owned(),
        needed,
        defined_versions,
        required_versions,
        interpreter,
        libc_family,
        has_rpath,
        has_runpath,
        filtered,
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
        glibc |= name == "libc.so.6" || crate::ldso::is_glibc_loader_name(name);
        musl |= name.starts_with("libc.musl-") || name.starts_with("ld-musl-");
    }
    match (glibc, musl) {
        (true, false) => LibcFamily::Glibc,
        (false, true) => LibcFamily::Musl,
        (true, true) => LibcFamily::Conflicting,
        (false, false) => LibcFamily::Unknown,
    }
}

/// A mapped segment's virtual address range and where it reads from.
struct LoadSegment {
    offset: u64,
    vaddr: u64,
    filesz: u64,
}

struct Object {
    file: File,
    file_len: u64,
    layout: Layout,
    programs: Vec<u8>,
}

impl Object {
    fn open(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        // Opened nonblocking, so a named pipe or device in a hostile
        // cache or counterpart directory cannot hang the open; the
        // regular-file check refuses it before any read waits.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0o4000)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid("the object is not a regular file"));
        }
        let file_len = metadata.len();
        let header = read_at(&mut file, file_len, 0, file_len.min(64) as usize)?;
        let layout = Layout::parse(&header)?;
        let program_len = bounded_table(layout.phnum, layout.phentsize)?;
        let programs = read_at(&mut file, file_len, layout.phoff, program_len)?;
        Ok(Self {
            file,
            file_len,
            layout,
            programs,
        })
    }

    /// The `PT_LOAD` segments, in address order. A segment whose file
    /// range leaves the file, whose mapped range overflows, that overlaps
    /// another segment's mapped range, or that shares a page with another
    /// segment while mapping it from a different file distance, is
    /// refused: the loader would not run it, or would map bytes the
    /// reader cannot attribute to one segment.
    fn load_segments(&self) -> io::Result<Vec<LoadSegment>> {
        let mut out = Vec::new();
        for entry in self.programs.chunks_exact(self.layout.phentsize.max(1)) {
            if u32_at(entry, 0) != PT_LOAD {
                continue;
            }
            let (offset, vaddr, filesz) = if self.layout.class64 {
                (u64_at(entry, 8), u64_at(entry, 16), u64_at(entry, 32))
            } else {
                (
                    u32_at(entry, 4) as u64,
                    u32_at(entry, 8) as u64,
                    u32_at(entry, 16) as u64,
                )
            };
            let end = offset
                .checked_add(filesz)
                .ok_or_else(|| invalid("load segment file range overflows"))?;
            if end > self.file_len {
                return Err(invalid("load segment lies outside the file"));
            }
            out.push(LoadSegment {
                offset,
                vaddr,
                filesz,
            });
        }
        out.sort_by_key(|s| s.vaddr);
        for pair in out.windows(2) {
            let (previous, next) = (&pair[0], &pair[1]);
            let previous_end = previous
                .vaddr
                .checked_add(previous.filesz)
                .ok_or_else(|| invalid("load segment addresses overflow"))?;
            if next.vaddr.checked_add(next.filesz).is_none() {
                return Err(invalid("load segment addresses overflow"));
            }
            if next.vaddr < previous_end {
                return Err(invalid("load segments overlap"));
            }
            // The loader maps a page once, so segments sharing one must
            // read it from the same file distance, or the bytes differ by
            // which mapping went down last.
            let same_page = previous.filesz > 0 && (previous_end - 1) / 4096 == next.vaddr / 4096;
            let distance = |s: &LoadSegment| s.offset.wrapping_sub(s.vaddr);
            if same_page && distance(previous) != distance(next) {
                return Err(invalid("load segments disagree within a shared page"));
            }
        }
        Ok(out)
    }

    /// The mapped address and length of the one `PT_DYNAMIC` segment, or
    /// `None` for an object without one, such as a static object. The
    /// table is named by its mapped address, which is where the loader
    /// reads it, not by the file offset a hostile segment could point at
    /// a decoy.
    fn dynamic_segment(&self) -> io::Result<Option<(u64, usize)>> {
        let mut found = None;
        for entry in self.programs.chunks_exact(self.layout.phentsize.max(1)) {
            if u32_at(entry, 0) != PT_DYNAMIC {
                continue;
            }
            if found.is_some() {
                return Err(invalid("multiple dynamic program headers"));
            }
            let (vaddr, filesz) = if self.layout.class64 {
                (u64_at(entry, 16), u64_at(entry, 32))
            } else {
                (u32_at(entry, 8) as u64, u32_at(entry, 16) as u64)
            };
            if filesz > MAX_TABLE as u64 {
                return Err(invalid("dynamic table is implausibly large"));
            }
            let len = usize::try_from(filesz).map_err(|_| invalid("dynamic length overflows"))?;
            found = Some((vaddr, len));
        }
        Ok(found)
    }

    /// The bytes a virtual address range of up to `max` bytes reads from:
    /// the covering `PT_LOAD` segment, bounded by what the segment maps.
    fn read_mapped(
        &mut self,
        loads: &[LoadSegment],
        vaddr: u64,
        max: usize,
    ) -> io::Result<Vec<u8>> {
        let segment = loads
            .iter()
            .find(|s| s.vaddr <= vaddr && vaddr < s.vaddr.saturating_add(s.filesz))
            .ok_or_else(|| invalid("address is outside the mapped segments"))?;
        let available = segment
            .vaddr
            .saturating_add(segment.filesz)
            .saturating_sub(vaddr);
        let len = max.min(usize::try_from(available).unwrap_or(max));
        let offset = segment
            .offset
            .checked_add(vaddr - segment.vaddr)
            .ok_or_else(|| invalid("load segment translation overflows"))?;
        read_at(&mut self.file, self.file_len, offset, len)
    }

    fn interpreter(&mut self, loads: &[LoadSegment]) -> io::Result<Option<String>> {
        let phentsize = self.layout.phentsize.max(1);
        let mut out = None;
        for start in (0..self.programs.len()).step_by(phentsize) {
            let end = (start + phentsize).min(self.programs.len());
            if end - start < phentsize {
                break;
            }
            let entry = &self.programs[start..end];
            if u32_at(entry, 0) != PT_INTERP {
                continue;
            }
            if out.is_some() {
                return Err(invalid("multiple ELF interpreters"));
            }
            let (vaddr, size) = if self.layout.class64 {
                (u64_at(entry, 16), u64_at(entry, 32))
            } else {
                (u32_at(entry, 8) as u64, u32_at(entry, 16) as u64)
            };
            if size == 0 || size > MAX_NAME as u64 {
                return Err(invalid("invalid interpreter length"));
            }
            let bytes = self.read_mapped(loads, vaddr, size as usize)?;
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

struct Layout {
    class64: bool,
    machine: u16,
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
                phoff: u64_at(header, 32),
                phentsize: u16_at(header, 54) as usize,
                phnum: u16_at(header, 56) as usize,
            }
        } else {
            Self {
                class64,
                machine: u16_at(header, 18),
                phoff: u32_at(header, 28) as u64,
                phentsize: u16_at(header, 42) as usize,
                phnum: u16_at(header, 44) as usize,
            }
        };
        if layout.phnum == 0xffff {
            return Err(invalid("extended ELF table numbering is unsupported"));
        }
        if layout.phnum > 0
            && (layout.phoff == 0 || layout.phentsize < if class64 { 56 } else { 32 })
        {
            return Err(invalid("invalid program header layout"));
        }
        Ok(layout)
    }
}

fn record(bytes: &[u8], pos: usize, len: usize) -> io::Result<&[u8]> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| invalid("ELF record range overflows"))?;
    bytes
        .get(pos..end)
        .ok_or_else(|| invalid("ELF record outside its table"))
}

fn advance(pos: usize, delta: usize, minimum: usize) -> io::Result<usize> {
    if delta < minimum {
        return Err(invalid("ELF chain overlaps or cycles"));
    }
    pos.checked_add(delta)
        .ok_or_else(|| invalid("ELF chain overflows"))
}

/// Refuse an auxiliary entry overlapping any header of its chain.
///
/// Linkers differ in where auxiliary entries go: GNU ld interleaves them
/// after their header, lld writes every header first. Only overlap with a
/// header is malformed. `headers` holds strictly ascending header offsets.
fn check_aux_placement(
    headers: &[usize],
    header_len: usize,
    auxes: &[usize],
    aux_len: usize,
) -> io::Result<()> {
    for &aux in auxes {
        let after = headers.partition_point(|&header| header + header_len <= aux);
        if headers
            .get(after)
            .is_some_and(|&header| header < aux + aux_len)
        {
            return Err(invalid("ELF auxiliary entry overlaps a header"));
        }
    }
    Ok(())
}

/// Read GNU version definitions from their table. Returns the defined
/// versions and the number of headers the chain holds.
fn parse_verdef(defs: &[u8], strings: &[u8]) -> io::Result<(VersionSet, usize)> {
    let mut out = VersionSet::new();
    if defs.is_empty() {
        return Ok((out, 0));
    }
    let mut pos = 0;
    let mut total_aux = 0usize;
    let mut headers = Vec::new();
    let mut auxes = Vec::new();
    let mut count = 0usize;
    for _ in 0..MAX_RECORDS {
        let entry = record(defs, pos, VERDEF_ENTRY)?;
        headers.push(pos);
        count += 1;
        if u16_at(entry, 0) != 1 {
            return Err(invalid("unsupported version definition format"));
        }
        let aux_count = u16_at(entry, 6) as usize;
        total_aux += aux_count;
        if aux_count == 0 || total_aux > MAX_RECORDS {
            return Err(invalid("invalid version definition auxiliary count"));
        }
        let mut aux_at = advance(pos, u32_at(entry, 12) as usize, VERDEF_ENTRY)?;
        for i in 0..aux_count {
            let aux = record(defs, aux_at, VERDAUX_ENTRY)?;
            auxes.push(aux_at);
            let name = string_at(strings, u32_at(aux, 0) as usize)?;
            if i == 0 {
                if u32_at(entry, 8) != elf_hash(&name) {
                    return Err(invalid("version definition hash disagrees with its name"));
                }
                if u16_at(entry, 2) & VER_FLG_BASE == 0 {
                    out.insert(name);
                }
            }
            let next = u32_at(aux, 4) as usize;
            if i + 1 == aux_count {
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
            check_aux_placement(&headers, VERDEF_ENTRY, &auxes, VERDAUX_ENTRY)?;
            return Ok((out, count));
        }
        pos = advance(pos, next, VERDEF_ENTRY)?;
    }
    Err(invalid("too many version definitions"))
}

/// The dynamic table's tags, before their string-table names resolve.
#[derive(Default)]
struct DynamicTags {
    needed: Vec<u64>,
    soname: Option<u64>,
    strtab: Option<u64>,
    has_rpath: bool,
    has_runpath: bool,
    filtered: bool,
    verdef: Option<u64>,
    verneed: Option<u64>,
    verneed_num: Option<u64>,
}

fn parse_dynamic_table(bytes: &[u8], class64: bool) -> io::Result<DynamicTags> {
    let stride = if class64 { 16 } else { 8 };
    if !bytes.len().is_multiple_of(stride) {
        return Err(invalid("truncated dynamic entry"));
    }
    let mut out = DynamicTags::default();
    for (i, entry) in bytes.chunks_exact(stride).enumerate() {
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
            DT_NEEDED => out.needed.push(value),
            DT_SONAME => {
                if out.soname.replace(value).is_some() {
                    return Err(invalid("duplicate SONAME"));
                }
            }
            DT_RPATH => out.has_rpath = true,
            DT_RUNPATH => out.has_runpath = true,
            DT_FILTER | DT_AUXILIARY => out.filtered = true,
            DT_STRTAB if out.strtab.replace(value).is_some() => {
                return Err(invalid("duplicate string table address"));
            }
            DT_VERDEF if out.verdef.replace(value).is_some() => {
                return Err(invalid("duplicate version definition address"));
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
    Err(invalid("dynamic table has no terminator"))
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
    // Structural pass: walk the header chain, collect every auxiliary
    // offset and refuse a malformed chain before any name decodes, so an
    // auxiliary that overlaps a header reports the overlap rather than
    // whatever its overlapping bytes happen to spell.
    let mut entries: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut total_aux = 0usize;
    let mut pos = 0usize;
    for _ in 0..MAX_RECORDS {
        let entry = record(bytes, pos, 16)?;
        if u16_at(entry, 0) != 1 {
            return Err(invalid("unsupported version requirement format"));
        }
        let aux_count = u16_at(entry, 2) as usize;
        total_aux = total_aux
            .checked_add(aux_count)
            .ok_or_else(|| invalid("version requirement count overflows"))?;
        if aux_count == 0 || total_aux > MAX_RECORDS {
            return Err(invalid("invalid version requirement auxiliary count"));
        }
        let mut aux_at = advance(pos, u32_at(entry, 8) as usize, 16)?;
        let mut auxes = Vec::new();
        for i in 0..aux_count {
            let aux = record(bytes, aux_at, 16)?;
            auxes.push(aux_at);
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
        entries.push((pos, auxes));
        let next = u32_at(entry, 12) as usize;
        if next == 0 {
            let headers: Vec<usize> = entries.iter().map(|(header, _)| *header).collect();
            let auxes: Vec<usize> = entries
                .iter()
                .flat_map(|(_, auxes)| auxes)
                .copied()
                .collect();
            check_aux_placement(&headers, 16, &auxes, 16)?;
            break;
        }
        pos = advance(pos, next, 16)?;
    }
    if entries.last().is_none() || u32_at(&bytes[entries.last().unwrap().0..], 12) != 0 {
        return Err(invalid("too many version requirements"));
    }
    // Content pass: the chain is sound, so decode every requirement.
    for (header, auxes) in &entries {
        let entry = &bytes[*header..*header + 16];
        let name = string_at(strings, u32_at(entry, 4) as usize)?;
        if !needed.contains(&name) {
            return Err(invalid(
                "version requirement does not name a needed dependency",
            ));
        }
        let versions = out.entry(name).or_insert_with(Vec::new);
        for aux_at in auxes {
            let aux = &bytes[*aux_at..*aux_at + 16];
            let name = string_at(strings, u32_at(aux, 8) as usize)?;
            if name.is_empty() {
                return Err(invalid("empty required version name"));
            }
            if u32_at(aux, 0) != elf_hash(&name) {
                return Err(invalid("version requirement hash disagrees with its name"));
            }
            versions.push(RequiredVersion {
                name,
                flags: u16_at(aux, 4),
            });
        }
    }
    Ok((out, entries.len()))
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

    /// The definition-count tag: fixtures emit it because real images
    /// carry it, though the reader walks the chain and ignores it.
    pub(crate) const DT_VERDEFNUM: u64 = 0x6fff_fffd;

    const DT_STRSZ: u64 = 10;

    /// A 64-bit ELF with defined versions from `defs` (each `(flags, name)`),
    /// a dynamic chain naming `needed`, and no extra tags. Either list may
    /// be empty.
    pub(crate) fn shared_object(defs: &[(u16, &str)], needed: &[&str]) -> Vec<u8> {
        shared_object_extra(defs, needed, &[])
    }

    /// [`shared_object`] carrying extra dynamic tags.
    pub(crate) fn shared_object_extra(
        defs: &[(u16, &str)],
        needed: &[&str],
        tags: &[(u64, u64)],
    ) -> Vec<u8> {
        shared_object_with_interp_extra(defs, needed, None, tags)
    }

    /// [`shared_object`] with a `PT_INTERP` program header naming `interp`,
    /// the way a libc names the loader it belongs with.
    pub(crate) fn shared_object_with_interp(
        defs: &[(u16, &str)],
        needed: &[&str],
        interp: Option<&str>,
    ) -> Vec<u8> {
        shared_object_with_interp_extra(defs, needed, interp, &[])
    }

    /// [`shared_object`] with a `PT_INTERP` and extra dynamic tags.
    pub(crate) fn shared_object_with_interp_extra(
        defs: &[(u16, &str)],
        needed: &[&str],
        interp: Option<&str>,
        tags: &[(u64, u64)],
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
        let verdef = build_verdef(defs, &name_offsets);

        let phnum = if interp.is_some() { 3 } else { 2 };
        let strings_at = 64 + phnum * 56;
        let verdef_at = strings_at + strings.len();
        let mut dynamic = Vec::new();
        let mut entry = |tag: u64, value: u64| {
            dynamic.extend_from_slice(&tag.to_le_bytes());
            dynamic.extend_from_slice(&value.to_le_bytes());
        };
        for off in needed_offsets {
            entry(DT_NEEDED, off as u64);
        }
        if !defs.is_empty() {
            entry(DT_VERDEF, verdef_at as u64);
            entry(DT_VERDEFNUM, defs.len() as u64);
        }
        entry(DT_STRTAB, strings_at as u64);
        entry(DT_STRSZ, strings.len() as u64);
        for (tag, value) in tags {
            entry(*tag, *value);
        }
        entry(DT_NULL, 0);
        layout_elf(true, 0, &strings, &verdef, &[], &dynamic, interp)
    }

    /// The version definition table for `name_offsets`, one header and one
    /// auxiliary entry each, carrying `flags`. Each header's hash is the
    /// System V hash of its name, as linkers emit it.
    fn build_verdef(defs: &[(u16, &str)], name_offsets: &[u32]) -> Vec<u8> {
        let mut verdef = Vec::new();
        for (i, (flags, _)) in defs.iter().enumerate() {
            let last = i + 1 == defs.len();
            verdef.extend_from_slice(&1u16.to_le_bytes());
            verdef.extend_from_slice(&flags.to_le_bytes());
            verdef.extend_from_slice(&(i as u16).to_le_bytes());
            verdef.extend_from_slice(&1u16.to_le_bytes());
            verdef.extend_from_slice(&elf_hash(defs[i].1).to_le_bytes());
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
        verdef
    }

    /// An ELF image of the given tables: header, a `PT_LOAD` covering the
    /// whole file at identity addresses, a `PT_DYNAMIC` naming the dynamic
    /// table, an optional `PT_INTERP`, then the table bodies. Virtual
    /// addresses equal file offsets.
    fn layout_elf(
        class64: bool,
        machine: u16,
        strings: &[u8],
        verdef: &[u8],
        verneed: &[u8],
        dynamic: &[u8],
        interp: Option<&str>,
    ) -> Vec<u8> {
        let (ehdr, phentsize) = if class64 {
            (64usize, 56usize)
        } else {
            (52, 32)
        };
        let phnum = if interp.is_some() { 3 } else { 2 };
        let strings_at = ehdr + phnum * phentsize;
        let verdef_at = strings_at + strings.len();
        let verneed_at = verdef_at + verdef.len();
        let dynamic_at = verneed_at + verneed.len();
        let mut len = dynamic_at + dynamic.len();
        let mut interp_at = 0;
        if let Some(text) = interp {
            interp_at = len;
            len = interp_at + text.len() + 1;
        }
        let mut v = vec![0u8; len];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = if class64 { 2 } else { 1 };
        v[5] = 1;
        v[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        v[18..20].copy_from_slice(&machine.to_le_bytes());
        if class64 {
            v[32..40].copy_from_slice(&(ehdr as u64).to_le_bytes()); // e_phoff
            v[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes());
            v[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
        } else {
            v[28..32].copy_from_slice(&(ehdr as u32).to_le_bytes()); // e_phoff
            v[42..44].copy_from_slice(&(phentsize as u16).to_le_bytes());
            v[44..46].copy_from_slice(&(phnum as u16).to_le_bytes());
        }
        let phdr = |v: &mut [u8], index: usize, kind: u32, offset: u64, vaddr: u64, filesz: u64| {
            let at = ehdr + index * phentsize;
            v[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            if class64 {
                v[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
                v[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
                v[at + 32..at + 40].copy_from_slice(&filesz.to_le_bytes());
            } else {
                v[at + 4..at + 8].copy_from_slice(&(offset as u32).to_le_bytes());
                v[at + 8..at + 12].copy_from_slice(&(vaddr as u32).to_le_bytes());
                v[at + 12..at + 16].copy_from_slice(&0u32.to_le_bytes());
                v[at + 16..at + 20].copy_from_slice(&(filesz as u32).to_le_bytes());
            }
        };
        phdr(&mut v, 0, PT_LOAD, 0, 0, len as u64);
        phdr(
            &mut v,
            1,
            PT_DYNAMIC,
            dynamic_at as u64,
            dynamic_at as u64,
            dynamic.len() as u64,
        );
        if let Some(text) = interp {
            phdr(
                &mut v,
                2,
                PT_INTERP,
                interp_at as u64,
                interp_at as u64,
                (text.len() + 1) as u64,
            );
            v[interp_at..interp_at + text.len()].copy_from_slice(text.as_bytes());
        }
        v[strings_at..strings_at + strings.len()].copy_from_slice(strings);
        v[verdef_at..verdef_at + verdef.len()].copy_from_slice(verdef);
        v[verneed_at..verneed_at + verneed.len()].copy_from_slice(verneed);
        v[dynamic_at..dynamic_at + dynamic.len()].copy_from_slice(dynamic);
        v
    }

    /// [`layout_elf`] for tests that assemble their own dynamic table.
    pub(crate) fn build_elf(strings: &[u8], verdef: &[u8], dynamic: &[u8]) -> Vec<u8> {
        layout_elf(true, 0, strings, verdef, &[], dynamic, None)
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

    /// A named pipe at `path`, for tests that must not block on it.
    pub(crate) fn make_fifo(path: &std::path::Path) {
        let ok = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo(1) must exist")
            .success();
        assert!(ok, "mkfifo failed for {}", path.display());
    }

    /// The bytes of a fixture [`facts_elf`] built, with the offsets a test
    /// needs to inspect or patch them at.
    pub(crate) struct Fixture {
        pub bytes: Vec<u8>,
        /// File offset of the string table.
        pub strtab_at: usize,
        /// File offset of the `DT_STRTAB` dynamic entry.
        pub strtab_tag_at: usize,
        /// File offset of the dynamic table.
        pub dynamic_at: usize,
        /// File offset of the version-need table.
        pub requirements_at: usize,
        /// Size of one dynamic entry.
        pub dynstride: usize,
    }

    pub(crate) fn intern(strings: &mut Vec<u8>, name: &str) -> u32 {
        let offset = strings.len() as u32;
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
        offset
    }

    /// A 64-bit or 32-bit ELF of `machine`, with a string table, an
    /// optional `DT_SONAME`, a dynamic chain naming `needed`, and a
    /// version need table demanding `requirements`, each entry a
    /// dependency file with its `(flags, name)` versions.
    pub(crate) fn facts_elf(
        class64: bool,
        machine: u16,
        soname: Option<&str>,
        needed: &[&str],
        requirements: &[(&str, &[(u16, &str)])],
    ) -> Fixture {
        facts_elf_with(class64, machine, soname, needed, requirements, &[])
    }

    /// [`facts_elf`] carrying extra dynamic tags.
    pub(crate) fn facts_elf_with(
        class64: bool,
        machine: u16,
        soname: Option<&str>,
        needed: &[&str],
        requirements: &[(&str, &[(u16, &str)])],
        tags: &[(u64, u64)],
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
        let (ehdr, phentsize, dynstride) = if class64 {
            (64usize, 56usize, 16usize)
        } else {
            (52, 32, 8)
        };
        let strtab_at = ehdr + 2 * phentsize;
        let strtab_entry_at = (needed.len() + usize::from(soname.is_some())) * dynstride;
        dyn_entry(DT_STRTAB, strtab_at as u64);
        dyn_entry(DT_STRSZ, strings.len() as u64);
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
                verneed[at..at + 4].copy_from_slice(&elf_hash(name).to_le_bytes());
                verneed[at + 4..at + 6].copy_from_slice(&flags.to_le_bytes());
                verneed[at + 6..at + 8].copy_from_slice(&((j + 2) as u16).to_le_bytes());
                verneed[at + 8..at + 12].copy_from_slice(&intern(&mut strings, name).to_le_bytes());
                if j + 1 < versions.len() {
                    verneed[at + 12..at + 16].copy_from_slice(&16u32.to_le_bytes());
                }
            }
        }
        let requirements_at = strtab_at + strings.len();
        if !requirements.is_empty() {
            dyn_entry(DT_VERNEED, requirements_at as u64);
            dyn_entry(DT_VERNEEDNUM, requirements.len() as u64);
        }
        for (tag, value) in tags {
            dyn_entry(*tag, *value);
        }
        dyn_entry(DT_NULL, 0);
        let dynamic_at = requirements_at + verneed.len();
        let bytes = layout_elf(class64, machine, &strings, &[], &verneed, &dynamic, None);
        Fixture {
            bytes,
            strtab_at,
            strtab_tag_at: dynamic_at + strtab_entry_at,
            dynamic_at,
            requirements_at,
            dynstride,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{DT_VERDEFNUM, build_elf, shared_object, temp_dir};
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
    fn without_program_headers_an_object_needs_and_defines_nothing() {
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
    fn truncation_anywhere_is_an_error_not_a_panic() {
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
        verdef.extend_from_slice(&elf_hash("FOO_1.0").to_le_bytes());
        verdef.extend_from_slice(&0u32.to_le_bytes());
        verdef.extend_from_slice(&(VERDEF_ENTRY as u32).to_le_bytes());
        verdef.extend_from_slice(&0u32.to_le_bytes());
        verdef.extend_from_slice(&9999u32.to_le_bytes());
        verdef.extend_from_slice(&0u32.to_le_bytes());
        // The tables sit after the ELF header and two program headers.
        let strings_at = 64 + 2 * 56;
        let verdef_at = strings_at + strings.len();
        let mut dynamic = Vec::new();
        dynamic.extend_from_slice(&DT_NEEDED.to_le_bytes());
        dynamic.extend_from_slice(&9999u64.to_le_bytes());
        dynamic.extend_from_slice(&DT_STRTAB.to_le_bytes());
        dynamic.extend_from_slice(&(strings_at as u64).to_le_bytes());
        dynamic.extend_from_slice(&DT_VERDEF.to_le_bytes());
        dynamic.extend_from_slice(&(verdef_at as u64).to_le_bytes());
        dynamic.extend_from_slice(&DT_VERDEFNUM.to_le_bytes());
        dynamic.extend_from_slice(&1u64.to_le_bytes());
        dynamic.extend_from_slice(&DT_NULL.to_le_bytes());
        dynamic.extend_from_slice(&0u64.to_le_bytes());
        let path = write_temp("bad.so", &build_elf(strings, &verdef, &dynamic));
        assert!(read(&path).is_err());
        assert!(read_needed(&path).is_err());
    }

    #[test]
    fn a_version_chain_that_leaves_its_table_is_an_error() {
        let elf = shared_object(&[(0, "FOO_1.0"), (0, "FOO_1.1")], &[]);
        let strings_len = 1 + "FOO_1.0".len() + 1 + "FOO_1.1".len() + 1;
        let verdef_at = 64 + 2 * 56 + strings_len;
        let mut runaway = elf.clone();
        runaway[verdef_at + 16..verdef_at + 20].copy_from_slice(&1000u32.to_le_bytes());
        let path = write_temp("runaway.so", &runaway);
        assert!(read(&path).is_err());

        // A chain ending early is fine: only the entries reached count.
        let mut short = elf.clone();
        short[verdef_at + 16..verdef_at + 20].copy_from_slice(&0u32.to_le_bytes());
        let path = write_temp("short.so", &short);
        assert_eq!(read(&path).unwrap(), set(&["FOO_1.0"]));
    }

    #[test]
    fn definitions_with_every_header_first_are_accepted() {
        let strings = b"\0libfoo.so.1\0FOO_1.0\0";
        let header = |flags: u16, index: u16, hash: u32, aux: u32, next: u32| {
            let mut v = Vec::new();
            v.extend_from_slice(&1u16.to_le_bytes());
            v.extend_from_slice(&flags.to_le_bytes());
            v.extend_from_slice(&index.to_le_bytes());
            v.extend_from_slice(&1u16.to_le_bytes());
            v.extend_from_slice(&hash.to_le_bytes());
            v.extend_from_slice(&aux.to_le_bytes());
            v.extend_from_slice(&next.to_le_bytes());
            v
        };
        let aux = |name: u32| [name.to_le_bytes(), 0u32.to_le_bytes()].concat();
        let headers = 2 * VERDEF_ENTRY as u32;
        let mut defs = header(
            VER_FLG_BASE,
            1,
            elf_hash("libfoo.so.1"),
            headers,
            VERDEF_ENTRY as u32,
        );
        defs.extend(header(
            0,
            2,
            elf_hash("FOO_1.0"),
            headers - VERDEF_ENTRY as u32 + 8,
            0,
        ));
        defs.extend(aux(1));
        defs.extend(aux(13));
        let (defined, count) = parse_verdef(&defs, strings).unwrap();
        assert_eq!(defined, set(&["FOO_1.0"]));
        assert_eq!(count, 2);

        defs[12..16].copy_from_slice(&(VERDEF_ENTRY as u32).to_le_bytes());
        assert!(parse_verdef(&defs, strings).is_err(), "aux over a header");
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
    use super::fixture::{
        facts_elf, facts_elf_with, make_fifo, shared_object, shared_object_with_interp, temp_dir,
    };
    use super::*;

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

    /// Rewrite an interleaved version-need chain into lld's layout: every
    /// header first, then every auxiliary entry in header order.
    fn lld_layout(verneed: &[u8]) -> Vec<u8> {
        let mut chains = Vec::new();
        let mut pos = 0;
        loop {
            let count = u16_at(verneed, pos + 2) as usize;
            let aux_at = pos + u32_at(verneed, pos + 8) as usize;
            chains.push((pos, &verneed[aux_at..aux_at + 16 * count]));
            match u32_at(verneed, pos + 12) {
                0 => break,
                next => pos += next as usize,
            }
        }
        let mut out = Vec::new();
        let mut aux_at = 16 * chains.len();
        for (i, (header, auxes)) in chains.iter().enumerate() {
            let mut entry = verneed[*header..header + 16].to_vec();
            entry[8..12].copy_from_slice(&((aux_at - 16 * i) as u32).to_le_bytes());
            let next = if i + 1 < chains.len() { 16u32 } else { 0 };
            entry[12..16].copy_from_slice(&next.to_le_bytes());
            out.extend_from_slice(&entry);
            aux_at += auxes.len();
        }
        for (_, auxes) in &chains {
            out.extend_from_slice(auxes);
        }
        assert_eq!(out.len(), verneed.len());
        out
    }

    #[test]
    fn lld_layout_with_every_header_first_is_accepted() {
        for class64 in [false, true] {
            let fixture = facts_elf(
                class64,
                62,
                None,
                &["libc.so.6", "libfoo.so"],
                &[
                    ("libfoo.so", &[(0, "FOO_1")]),
                    ("libc.so.6", &[(0, "GLIBC_2.17"), (2, "GLIBC_2.34")]),
                ],
            );
            let expected = inspect(&fixture.bytes).unwrap().required_versions;
            let at = fixture.requirements_at;
            let end = fixture.dynamic_at;
            let mut bytes = fixture.bytes.clone();
            let relaid = lld_layout(&bytes[at..end]);
            assert_ne!(relaid, bytes[at..end], "the layouts differ");
            bytes[at..end].copy_from_slice(&relaid);
            assert_eq!(inspect(&bytes).unwrap().required_versions, expected);

            let mut overlapping = bytes.clone();
            // Point the first header's auxiliary block at the second
            // header's slot: the structural pass must report the overlap
            // before whatever those bytes decode as.
            overlapping[at + 8..at + 12].copy_from_slice(&16u32.to_le_bytes());
            let error = inspect(&overlapping).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("overlaps a header"), "{error}");
        }
    }

    #[test]
    fn lld_linked_objects_on_the_host_parse_when_present() {
        let path = Path::new("/usr/lib/LLVMgold.so");
        if !path.is_file() {
            return;
        }
        let facts = read_facts(path).unwrap();
        assert!(!facts.required_versions.is_empty());
    }

    /// Dynamic tables whose tags lie: names outside the string table,
    /// tables outside the mapped segments, counts that disagree with
    /// their chain, a missing terminator and duplicated tags all refuse.
    #[test]
    fn malformed_dynamic_counts_addresses_and_entries_fail() {
        let fixture = facts_elf(
            true,
            62,
            None,
            &["libc.so.6"],
            &[("libc.so.6", &[(0, "GLIBC_2.17")])],
        );
        // Entries: 0 NEEDED, 1 STRTAB, 2 STRSZ, 3 VERNEED, 4 VERNEEDNUM,
        // 5 NULL. Each lie is written to the entry's value field, except
        // the terminator, whose tag must say the table goes on.
        let at = |index: usize| fixture.dynamic_at + index * fixture.dynstride;
        for (index, tag, value) in [
            (0usize, None, Some(u64::MAX)),
            (1, None, Some(u64::MAX)),
            (3, None, Some(u64::MAX)),
            (4, None, Some(2)),
            (4, None, Some(0)),
            (5, Some(DT_NEEDED), None),
        ] {
            let mut bytes = fixture.bytes.clone();
            let offset = at(index);
            if let Some(tag) = tag {
                bytes[offset..offset + 8].copy_from_slice(&tag.to_le_bytes());
            }
            if let Some(value) = value {
                bytes[offset + 8..offset + 16].copy_from_slice(&value.to_le_bytes());
            }
            assert_invalid(&bytes);
        }
        let duplicate = |tags: &[(u64, u64)]| {
            inspect(&facts_elf_with(true, 62, Some("libreal.so.1"), &[], &[], tags).bytes).is_err()
        };
        assert!(duplicate(&[(DT_SONAME, 1)]), "duplicate SONAME");
        assert!(
            duplicate(&[(DT_VERNEED, 4096)]),
            "a lone version table is read, so this only checks no panic"
        );
        let two_needs = facts_elf_with(
            true,
            62,
            None,
            &["libc.so.6"],
            &[("libc.so.6", &[(0, "GLIBC_2.17")])],
            &[],
        )
        .bytes;
        assert!(inspect(&two_needs).is_ok());
    }

    /// A section table that lies about the string table cannot change the
    /// facts: they come from the dynamic table and the segments that map
    /// it, which is what the loader reads.
    #[test]
    fn a_section_table_that_lies_cannot_change_the_facts() {
        let fixture = facts_elf(true, 62, None, &["libevil.so.1"], &[]);
        let mut bytes = fixture.bytes.clone();
        let decoy = b"\0libgood.so.1\0";
        let decoy_at = bytes.len();
        bytes.extend_from_slice(decoy);
        let shoff = bytes.len();
        let shentsize = 64usize;
        bytes.resize(shoff + 3 * shentsize, 0);
        for (i, kind) in [(0usize, 0u32), (1, 3u32), (2, 6u32)] {
            let at = shoff + i * shentsize;
            bytes[at + 4..at + 8].copy_from_slice(&kind.to_le_bytes());
            bytes[at + 16..at + 24].copy_from_slice(&(decoy_at as u64).to_le_bytes());
            bytes[at + 24..at + 32].copy_from_slice(&(decoy_at as u64).to_le_bytes());
            bytes[at + 32..at + 40].copy_from_slice(&(decoy.len() as u64).to_le_bytes());
        }
        bytes[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
        bytes[58..60].copy_from_slice(&(shentsize as u16).to_le_bytes());
        bytes[60..62].copy_from_slice(&3u16.to_le_bytes());
        let facts = inspect(&bytes).unwrap();
        assert_eq!(facts.needed, ["libevil.so.1"], "sections do not speak");
    }

    /// The dynamic table's own addresses are the truth: a `DT_STRTAB`
    /// moved to another region moves the names with it, the way the
    /// loader reads them.
    #[test]
    fn a_moved_string_table_moves_the_names_with_it() {
        let fixture = facts_elf(true, 62, None, &["libevil.so.1"], &[]);
        let mut bytes = fixture.bytes.clone();
        let decoy = b"\0libgood.so.1\0";
        let decoy_at = bytes.len();
        bytes.extend_from_slice(decoy);
        // The load segment grows with the image, so the moved table is
        // mapped, the way a real binary maps its regions.
        let grown = bytes.len() as u64;
        bytes[64 + 32..64 + 40].copy_from_slice(&grown.to_le_bytes());
        let at = fixture.strtab_tag_at;
        bytes[at + 8..at + 16].copy_from_slice(&(decoy_at as u64).to_le_bytes());
        let facts = inspect(&bytes).unwrap();
        assert_eq!(facts.needed, ["libgood.so.1"], "the dynamic table speaks");
    }

    /// A load segment whose virtual addresses are not its file offsets
    /// is mapped the way the loader maps it: addresses resolve through
    /// the segment, whatever the distance between the two spaces.
    #[test]
    fn an_address_space_that_is_not_the_file_is_mapped_through_the_segments() {
        let fixture = facts_elf(
            true,
            62,
            None,
            &["libc.so.6"],
            &[("libc.so.6", &[(0, "GLIBC_2.17")])],
        );
        let shift = 0x400000u64;
        let mut bytes = fixture.bytes.clone();
        let load = 64usize;
        bytes[load + 16..load + 24].copy_from_slice(&shift.to_le_bytes());
        let dynamic = 64usize + 56usize;
        bytes[dynamic + 16..dynamic + 24]
            .copy_from_slice(&((fixture.dynamic_at as u64) + shift).to_le_bytes());
        let strtab_tag = fixture.strtab_tag_at;
        bytes[strtab_tag + 8..strtab_tag + 16]
            .copy_from_slice(&((fixture.strtab_at as u64) + shift).to_le_bytes());
        let verneed_tag = fixture.dynamic_at + 3 * fixture.dynstride;
        bytes[verneed_tag + 8..verneed_tag + 16]
            .copy_from_slice(&((fixture.requirements_at as u64) + shift).to_le_bytes());
        let facts = inspect(&bytes).unwrap();
        assert_eq!(facts.needed, ["libc.so.6"]);
        assert_eq!(facts.required_versions["libc.so.6"][0].name, "GLIBC_2.17");
    }

    /// An ELF32 program header's `p_paddr` sits between its virtual
    /// address and its file size; the reader takes the size from the
    /// ABI slot, so a load segment with a nonzero physical address is
    /// read as the loader would read it.
    #[test]
    fn an_elf32_paddr_is_not_taken_for_its_file_size() {
        let fixture = facts_elf(false, 3, None, &["libc.so.6"], &[]);
        let mut bytes = fixture.bytes.clone();
        let at = 52usize + 12usize;
        bytes[at..at + 4].copy_from_slice(&0x0040_0000u32.to_le_bytes());
        let facts = inspect(&bytes).unwrap();
        assert_eq!(facts.needed, ["libc.so.6"]);
    }

    /// The lookup-path and filter tags land in the facts: an rpath
    /// without a runpath, both together, a runpath alone, a filter and
    /// an auxiliary, each as the closure walk must see them.
    #[test]
    fn rpath_runpath_and_filter_tags_become_facts() {
        let facts =
            inspect(&facts_elf_with(true, 62, None, &[], &[], &[(DT_RPATH, 0)]).bytes).unwrap();
        assert!(facts.has_rpath);
        assert!(!facts.has_runpath);
        assert!(!facts.filtered);

        let facts = inspect(
            &facts_elf_with(true, 62, None, &[], &[], &[(DT_RPATH, 0), (DT_RUNPATH, 0)]).bytes,
        )
        .unwrap();
        assert!(facts.has_rpath);
        assert!(facts.has_runpath);

        let facts =
            inspect(&facts_elf_with(true, 62, None, &[], &[], &[(DT_RUNPATH, 0)]).bytes).unwrap();
        assert!(!facts.has_rpath);
        assert!(facts.has_runpath);

        for tag in [DT_FILTER, DT_AUXILIARY] {
            let facts =
                inspect(&facts_elf_with(true, 62, None, &[], &[], &[(tag, 0)]).bytes).unwrap();
            assert!(facts.filtered, "{tag}");
        }

        // The ABI values of the filter tags, written as literals so a
        // constant slip cannot pass silently. Tag 30 and 32 are
        // DT_FLAGS and DT_PREINIT_ARRAY, which ordinary images carry
        // and which are not filters.
        for tag in [0x7fff_ffffu64, 0x7fff_fffd] {
            let facts =
                inspect(&facts_elf_with(true, 62, None, &[], &[], &[(tag, 0)]).bytes).unwrap();
            assert!(facts.filtered, "{tag:#x}");
        }
        let facts =
            inspect(&facts_elf_with(true, 62, None, &[], &[], &[(30, 0), (32, 0)]).bytes).unwrap();
        assert!(!facts.filtered);

        let facts = inspect(&facts_elf(true, 62, None, &[], &[]).bytes).unwrap();
        assert!(!facts.has_rpath && !facts.has_runpath && !facts.filtered);
    }

    /// A version entry whose System V hash disagrees with its name is
    /// malformed: the loader's match requires both, so such a demand
    /// can never be satisfied and must refuse here, not admit.
    #[test]
    fn version_hashes_must_match_their_names() {
        assert_eq!(elf_hash("DEP_1"), 0x48a621);
        let fixture = facts_elf(
            true,
            62,
            None,
            &["libc.so.6"],
            &[("libc.so.6", &[(0, "GLIBC_2.17")])],
        );
        let at = fixture.requirements_at;
        let mut bytes = fixture.bytes.clone();
        bytes[at + 16..at + 20].copy_from_slice(&(elf_hash("GLIBC_2.17") ^ 1).to_le_bytes());
        assert_invalid(&bytes);

        let elf = shared_object(&[(0, "FOO_1.0")], &[]);
        let strings_len = 1 + "FOO_1.0".len() + 1;
        let verdef_at = 64 + 2 * 56 + strings_len;
        let mut broken = elf.clone();
        broken[verdef_at + 8..verdef_at + 12].copy_from_slice(&0u32.to_le_bytes());
        assert_invalid(&broken);
    }

    /// A load segment whose file range leaves the file or whose
    /// addresses overflow is refused, not a panic: the loader could
    /// not map it, and hostile metadata must not crash the reader.
    #[test]
    fn a_load_segment_outside_the_file_or_overflowing_is_refused() {
        let fixture = facts_elf(true, 62, None, &["libc.so.6"], &[]);
        let mut bytes = fixture.bytes.clone();
        bytes[64 + 8..64 + 16].copy_from_slice(&0xffffffffffffff00u64.to_le_bytes());
        assert_invalid(&bytes);

        let mut bytes = fixture.bytes.clone();
        bytes[64 + 16..64 + 24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_invalid(&bytes);
    }

    /// The dynamic table is read where the segments map it, not at the
    /// file offset a hostile `PT_DYNAMIC` could point at a decoy: the
    /// loader reads the mapped table, and so must the facts.
    #[test]
    fn a_decoy_dynamic_offset_changes_nothing() {
        let fixture = facts_elf(true, 62, None, &["libc.so.6"], &[]);
        let mut bytes = fixture.bytes.clone();
        let decoy_at = bytes.len();
        bytes.extend_from_slice(&[0u8; 16 * 8]);
        let dynamic_phdr = 64 + 56;
        bytes[dynamic_phdr + 8..dynamic_phdr + 16]
            .copy_from_slice(&(decoy_at as u64).to_le_bytes());
        let facts = inspect(&bytes).unwrap();
        assert_eq!(facts.needed, ["libc.so.6"]);
        assert!(facts.required_versions.is_empty());
    }

    /// Two `PT_LOAD` segments at `(offset, vaddr, filesz)` with a
    /// dynamic table and string table placed at their mapped addresses.
    fn twin_load_elf(
        first: (u64, u64, u64),
        second: (u64, u64, u64),
        dynamic_vaddr: u64,
        entries: &[(u64, u64)],
        strtab: (u64, &[u8]),
    ) -> Vec<u8> {
        let ehdr = 64usize;
        let phentsize = 56usize;
        let phnum = 3usize;
        let dynamic = {
            let mut bytes = Vec::new();
            for (tag, value) in entries {
                bytes.extend_from_slice(&tag.to_le_bytes());
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            bytes.extend_from_slice(&0u64.to_le_bytes());
            bytes.extend_from_slice(&0u64.to_le_bytes());
            bytes
        };
        let end = |s: &(u64, u64, u64)| s.0.saturating_add(s.2);
        let len = end(&first)
            .max(end(&second))
            .max(strtab.0 + strtab.1.len() as u64)
            .max(dynamic_vaddr + dynamic.len() as u64);
        let mut v = vec![0u8; len as usize];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = 2;
        v[5] = 1;
        v[16..18].copy_from_slice(&3u16.to_le_bytes());
        v[18..20].copy_from_slice(&62u16.to_le_bytes());
        v[32..40].copy_from_slice(&(ehdr as u64).to_le_bytes());
        v[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes());
        v[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
        let phdr = |v: &mut [u8], index: usize, kind: u32, offset: u64, vaddr: u64, filesz: u64| {
            let at = ehdr + index * phentsize;
            v[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            v[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
            v[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
            v[at + 32..at + 40].copy_from_slice(&filesz.to_le_bytes());
        };
        phdr(&mut v, 0, 1, first.0, first.1, first.2);
        phdr(&mut v, 1, 1, second.0, second.1, second.2);
        phdr(
            &mut v,
            2,
            2,
            dynamic_vaddr,
            dynamic_vaddr,
            dynamic.len() as u64,
        );
        v[strtab.0 as usize..strtab.0 as usize + strtab.1.len()].copy_from_slice(strtab.1);
        v[dynamic_vaddr as usize..dynamic_vaddr as usize + dynamic.len()].copy_from_slice(&dynamic);
        v
    }

    /// Load segments whose mapped ranges overlap are refused: the
    /// loader maps the later one over the earlier, so the reader could
    /// decode names from bytes the loader never serves.
    #[test]
    fn overlapping_load_segments_are_refused() {
        let bytes = twin_load_elf(
            (0x1000, 0x1000, 0x2000),
            (0x4000, 0x2000, 0x2000),
            0x2500,
            &[(5, 0x2000), (1, 1)],
            (0x2000, b"\0libgood.so.1\0"),
        );
        assert_invalid(&bytes);
    }

    /// Segments that share a page the way the linker's packed layouts
    /// do, mapping it from the same file distance, are accepted: the
    /// shared page holds the same bytes from either mapping.
    #[test]
    fn segments_sharing_a_page_from_one_distance_are_accepted() {
        let bytes = twin_load_elf(
            (0x1000, 0x1000, 0x10),
            (0x1010, 0x1010, 0x60),
            0x1010,
            &[(5, 0x1050), (1, 1)],
            (0x1050, b"\0libgood.so.1\0"),
        );
        let facts = inspect(&bytes).unwrap();
        assert_eq!(facts.needed, ["libgood.so.1"]);
    }

    /// A counterpart dependency that is a named pipe is refused at
    /// once, not hung on: a hostile cache entry must not stall the
    /// resolver.
    #[test]
    fn a_fifo_where_an_object_is_expected_is_refused_without_hanging() {
        let dir = temp_dir("fifo");
        let path = dir.join("libfifo.so");
        make_fifo(&path);
        let started = std::time::Instant::now();
        let result = read_facts(&path);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(result.is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the open must not wait on the pipe"
        );
    }

    /// An interpreter past its bounds is refused, whether it holds a
    /// stray interior NUL or claims a length no file has.
    #[test]
    fn an_interp_past_its_bounds_is_an_error() {
        let phnum_interp = 64 + 2 * 56;
        let mut bytes = shared_object_with_interp(&[], &[], Some("/lib/ld-musl-x86_64.so.1"));
        let interp_at = u64_at(&bytes[phnum_interp..], 8) as usize;
        bytes[interp_at] = 0;
        assert_invalid(&bytes);
        let mut bytes = shared_object_with_interp(&[], &[], Some("/lib/ld-musl-x86_64.so.1"));
        bytes[phnum_interp + 32..phnum_interp + 40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_invalid(&bytes);
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
        assert!(parse_dynamic_table(&dynamic, true).is_err());
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
