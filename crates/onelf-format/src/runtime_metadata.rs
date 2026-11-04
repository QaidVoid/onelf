//! Bounded binary transport for normalized runtime rules and entrypoint sets.
//!
//! Both formats have a 16-byte header: magic (`ORUL` or `OSET`), little-endian
//! u16 version 1, zero u16 reserved flags, u32 payload length, and u32 aggregate
//! record count. Every string has a u32 byte length. Every typed record has a
//! u32 byte length followed by a u8 tag and its fixed-order fields. Counts are
//! u32. Lengths exclude their own prefix. All integers are little-endian.
//!
//! ORUL contains a count and rule records (tag 1): descriptor ID, binding flag,
//! optional pin record (tag 2: set name, 32-byte BLAKE3), path count and path
//! records, then hook count and hook records. A path (tag 3) contains ID, kind,
//! selection, inheritance, candidate count and candidate records, then alias
//! count and alias strings. A candidate (tag 4) contains base and relative path.
//! A hook (tag 5) contains its registry tag. Binding flags are 0 or 1. Kind tags
//! are file=1 and directory=2, selection tags are first=1, all=2 and
//! first-per-library-root=3, inheritance tags are preserve=1 and replace=2,
//! and base tags are package-root=1 and library-root=2. Hook tags 1 through 5
//! name ALSA natural sorting, GDK cache absolutising, fontconfig generation,
//! GIO module paths and Tcl/Tk libraries, respectively.
//!
//! OSET contains a pin count and pin records (tag 1: set name, full BLAKE3),
//! then an entrypoint count and selection records (tag 2: entrypoint name,
//! reference count, set-name strings). Empty selections and empty tables are
//! structurally valid. Consumers must check the final manifest and pin tables
//! with [`EntrypointSets::validate_against`] before use.
//!
//! Aggregate records include rules, bindings, paths, candidates, aliases, hooks,
//! pins, entrypoints and set references. Strings that identify their containing
//! record do not count separately. The typed grammar has at most four levels
//! including leaf strings. There is no recursive or extensible value grammar.
//!
//! Encoding sorts rules by scoped identity and OSET tables and references by
//! name. Runtime path, candidate, alias and hook order remains significant.
//! Decoding rejects noncanonical ordering and nonnormalized paths. Every
//! accepted byte string therefore re-encodes byte-for-byte. Absence handling,
//! integrity verification, acquisition and runtime activation belong to callers.

use std::collections::BTreeSet;
use std::fmt;

use crate::rules::{
    EnvironmentName, Inheritance, MAX_ID_BYTES, MAX_PATH_BYTES, MAX_RECORDS, PathSelection,
    RelativePath, ResourceKind, RuleError, RuleId, RuntimeBase, RuntimeCandidate, RuntimeHookId,
    RuntimePathGroup, RuntimeRules,
};

/// Maximum complete encoded metadata file size, including its header.
pub const MAX_METADATA_BYTES: usize = 1024 * 1024;
/// Encoded header size. Existing manifest and entrypoint layouts are unaffected.
pub const HEADER_BYTES: usize = 16;
/// Supported transport schema version.
pub const METADATA_VERSION: u16 = 1;

/// A bounded diagnostic for malformed or unsupported metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataError(String);

impl MetadataError {
    fn new(reason: &str) -> Self {
        Self(reason.chars().take(768).collect())
    }
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MetadataError {}

impl From<RuleError> for MetadataError {
    fn from(value: RuleError) -> Self {
        Self::new(&value.to_string())
    }
}

/// A declared set identity and its full pinned artifact hash.
///
/// Different names may intentionally pin the same artifact. A name cannot have
/// multiple pins within one table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SetPin {
    /// Canonical declaration name, validated by [`RuleId::set_name`].
    pub name: RuleId,
    /// Full BLAKE3 artifact digest, not a truncated identifier.
    pub hash: [u8; 32],
}

/// One descriptor's runtime behavior, optionally scoped to a selected set pin.
///
/// The pair `(id, binding)` is the record identity. A descriptor may appear for
/// multiple pins without synthetic descriptor names. A bound record describes
/// prepared application resources belonging to that exact pin. Consumers must
/// activate it only when that pin is selected. No source paths or build hooks
/// are transported, and this type does not activate or discover resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRuleRecord {
    /// Original descriptor identity.
    pub id: RuleId,
    /// Origin of prepared per-pin resources, or `None` for unbound mappings.
    pub binding: Option<SetPin>,
    /// Normalized presence mappings and closed runtime hook IDs.
    pub runtime: RuntimeRules,
}

/// A complete runtime-rule transport file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeRuleMetadata {
    /// Runtime records. Encoding sorts scoped identities without reordering paths.
    pub rules: Vec<RuntimeRuleRecord>,
}

/// An entrypoint's complete named-set selection, including explicit empty lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntrypointSelection {
    /// Existing manifest name. UTF-8 is allowed, with at most 128 bytes and no NUL.
    pub name: String,
    /// Exact selected declaration names. Duplicate and unknown names are invalid.
    pub sets: Vec<RuleId>,
}

/// A canonical declared-pin table and complete name-to-selection mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntrypointSets {
    /// All declared sets, including sets selected by no entrypoint.
    pub pins: Vec<SetPin>,
    /// One record per final manifest entrypoint, checked externally before use.
    pub entrypoints: Vec<EntrypointSelection>,
}

impl RuntimeRuleMetadata {
    /// Validate aggregate bounds, runtime groups and duplicate scoped identities.
    pub fn validate(&self) -> Result<(), MetadataError> {
        self.record_count()?;
        let mut identities = BTreeSet::new();
        for rule in &self.rules {
            if !identities.insert((&rule.id, &rule.binding)) {
                return Err(MetadataError::new(
                    "runtime rules: duplicate scoped descriptor identity",
                ));
            }
            if let Some(pin) = &rule.binding {
                RuleId::set_name(pin.name.as_str())?;
            }
            rule.runtime.validate()?;
        }
        Ok(())
    }

    fn record_count(&self) -> Result<usize, MetadataError> {
        let mut budget = Budget::default();
        budget.add(self.rules.len())?;
        for rule in &self.rules {
            budget.add(usize::from(rule.binding.is_some()))?;
            budget.add(rule.runtime.paths.len())?;
            budget.add(rule.runtime.hooks.len())?;
            for path in &rule.runtime.paths {
                budget.add(path.candidates.len())?;
                budget.add(path.variables.len())?;
            }
        }
        Ok(budget.0)
    }

    /// Check bound prepared-resource origins against a validated selection table.
    ///
    /// This checks declarations, not the active selection or actual filesystem.
    pub fn validate_bindings(&self, sets: &EntrypointSets) -> Result<(), MetadataError> {
        self.validate()?;
        sets.validate()?;
        let pins: BTreeSet<_> = sets.pins.iter().collect();
        for rule in &self.rules {
            if rule.binding.as_ref().is_some_and(|pin| !pins.contains(pin)) {
                return Err(MetadataError::new(
                    "runtime binding: unknown set or mismatched full pin",
                ));
            }
        }
        Ok(())
    }

    /// Encode canonical ORUL bytes, rejecting files above 1 MiB.
    pub fn encode(&self) -> Result<Vec<u8>, MetadataError> {
        self.validate()?;
        let mut writer = Writer::new(*b"ORUL", self.record_count()?)?;
        let mut rules: Vec<_> = self.rules.iter().collect();
        rules.sort_by_key(|rule| (&rule.id, &rule.binding));
        writer.count(rules.len())?;
        for rule in rules {
            let start = writer.start_record(1)?;
            writer.string(rule.id.as_str())?;
            writer.byte(u8::from(rule.binding.is_some()))?;
            if let Some(pin) = &rule.binding {
                write_pin(&mut writer, pin, 2)?;
            }
            writer.count(rule.runtime.paths.len())?;
            for path in &rule.runtime.paths {
                write_path(&mut writer, path)?;
            }
            writer.count(rule.runtime.hooks.len())?;
            for hook in &rule.runtime.hooks {
                let hook_start = writer.start_record(5)?;
                writer.byte(match hook {
                    RuntimeHookId::AlsaConfigPathNaturalSort => 1,
                    RuntimeHookId::GdkCacheAbsolutise => 2,
                    RuntimeHookId::FontconfigGenerate => 3,
                    RuntimeHookId::GioModulePaths => 4,
                    RuntimeHookId::TclTkLibraries => 5,
                })?;
                writer.end_record(hook_start)?;
            }
            writer.end_record(start)?;
        }
        writer.finish()
    }

    /// Decode a present ORUL file with no integrity or filesystem side effects.
    pub fn decode(bytes: &[u8]) -> Result<Self, MetadataError> {
        let (mut reader, expected) = read_header(bytes, b"ORUL")?;
        let mut budget = Budget::default();
        let count = reader.count(5, &mut budget)?;
        let mut rules = Vec::new();
        for _ in 0..count {
            let mut record = reader.record(1)?;
            let id = read_id(&mut record)?;
            let binding = match record.byte()? {
                0 => None,
                1 => {
                    budget.add(1)?;
                    Some(read_pin(&mut record, 2)?)
                }
                _ => return Err(MetadataError::new("runtime binding: unknown flag")),
            };
            let paths_count = record.count(5, &mut budget)?;
            let mut paths = Vec::new();
            for _ in 0..paths_count {
                paths.push(read_path(&mut record, &mut budget)?);
            }
            let hooks_count = record.count(6, &mut budget)?;
            let mut hooks = Vec::new();
            for _ in 0..hooks_count {
                let mut hook = record.record(5)?;
                hooks.push(match hook.byte()? {
                    1 => RuntimeHookId::AlsaConfigPathNaturalSort,
                    2 => RuntimeHookId::GdkCacheAbsolutise,
                    3 => RuntimeHookId::FontconfigGenerate,
                    4 => RuntimeHookId::GioModulePaths,
                    5 => RuntimeHookId::TclTkLibraries,
                    _ => return Err(MetadataError::new("runtime hook: unknown registry tag")),
                });
                hook.finish()?;
            }
            record.finish()?;
            rules.push(RuntimeRuleRecord {
                id,
                binding,
                runtime: RuntimeRules { paths, hooks },
            });
        }
        reader.finish()?;
        budget.finish(expected)?;
        let value = Self { rules };
        value.validate()?;
        if value
            .rules
            .windows(2)
            .any(|pair| (&pair[0].id, &pair[0].binding) >= (&pair[1].id, &pair[1].binding))
        {
            return Err(MetadataError::new(
                "runtime rules: noncanonical scoped identity order",
            ));
        }
        Ok(value)
    }
}

impl EntrypointSets {
    /// Validate aggregate bounds, unique names and internal pin references.
    pub fn validate(&self) -> Result<(), MetadataError> {
        self.record_count()?;
        let mut pins = BTreeSet::new();
        for pin in &self.pins {
            RuleId::set_name(pin.name.as_str())?;
            if !pins.insert(&pin.name) {
                return Err(MetadataError::new(
                    "pin table: duplicate set name or conflicting pins",
                ));
            }
        }
        let mut names = BTreeSet::new();
        for entry in &self.entrypoints {
            validate_entrypoint_name(&entry.name)?;
            if !names.insert(&entry.name) {
                return Err(MetadataError::new(
                    "selection table: duplicate entrypoint name",
                ));
            }
            let mut selected = BTreeSet::new();
            for name in &entry.sets {
                if !pins.contains(name) {
                    return Err(MetadataError::new("selection: unknown declared set name"));
                }
                if !selected.insert(name) {
                    return Err(MetadataError::new("selection: duplicate set reference"));
                }
            }
        }
        Ok(())
    }

    fn record_count(&self) -> Result<usize, MetadataError> {
        let mut budget = Budget::default();
        budget.add(self.pins.len())?;
        budget.add(self.entrypoints.len())?;
        for entry in &self.entrypoints {
            budget.add(entry.sets.len())?;
        }
        Ok(budget.0)
    }

    /// Require exact equality with the final manifest names and actual set pins.
    ///
    /// Input order is immaterial. Missing, extra, duplicate or changed declarations
    /// are errors. An empty metadata entrypoint table does not match a nonempty
    /// manifest. This performs no artifact reads or acquisition.
    pub fn validate_against(
        &self,
        manifest_names: &[&str],
        actual_pins: &[SetPin],
    ) -> Result<(), MetadataError> {
        self.validate()?;
        if manifest_names.len() > MAX_RECORDS || actual_pins.len() > MAX_RECORDS {
            return Err(MetadataError::new(
                "external tables: exceeds MAX_RECORDS (4096)",
            ));
        }
        let mut expected_names = BTreeSet::new();
        for name in manifest_names {
            validate_entrypoint_name(name)?;
            if !expected_names.insert(*name) {
                return Err(MetadataError::new("manifest: duplicate entrypoint name"));
            }
        }
        let names: BTreeSet<_> = self
            .entrypoints
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        if names != expected_names {
            return Err(MetadataError::new(
                "selection table: does not match final manifest entrypoint names",
            ));
        }
        let mut pin_names = BTreeSet::new();
        for pin in actual_pins {
            if !pin_names.insert(&pin.name) {
                return Err(MetadataError::new(
                    "actual pins: duplicate set name or conflicting pins",
                ));
            }
        }
        let expected_pins: BTreeSet<_> = actual_pins.iter().collect();
        let pins: BTreeSet<_> = self.pins.iter().collect();
        if pins != expected_pins {
            return Err(MetadataError::new(
                "pin table: does not match actual declared full pins",
            ));
        }
        Ok(())
    }

    /// Encode canonical OSET bytes using full hashes and sorted name lists.
    pub fn encode(&self) -> Result<Vec<u8>, MetadataError> {
        self.validate()?;
        let mut writer = Writer::new(*b"OSET", self.record_count()?)?;
        let mut pins: Vec<_> = self.pins.iter().collect();
        pins.sort_by_key(|pin| &pin.name);
        writer.count(pins.len())?;
        for pin in pins {
            write_pin(&mut writer, pin, 1)?;
        }
        let mut entrypoints: Vec<_> = self.entrypoints.iter().collect();
        entrypoints.sort_by_key(|entry| &entry.name);
        writer.count(entrypoints.len())?;
        for entry in entrypoints {
            let start = writer.start_record(2)?;
            writer.string(&entry.name)?;
            let mut sets: Vec<_> = entry.sets.iter().collect();
            sets.sort();
            writer.count(sets.len())?;
            for set in sets {
                writer.string(set.as_str())?;
            }
            writer.end_record(start)?;
        }
        writer.finish()
    }

    /// Decode a present OSET file, rejecting noncanonical or inconsistent tables.
    pub fn decode(bytes: &[u8]) -> Result<Self, MetadataError> {
        let (mut reader, expected) = read_header(bytes, b"OSET")?;
        let mut budget = Budget::default();
        let pins_count = reader.count(42, &mut budget)?;
        let mut pins = Vec::new();
        for _ in 0..pins_count {
            pins.push(read_pin(&mut reader, 1)?);
        }
        let entries_count = reader.count(14, &mut budget)?;
        let mut entrypoints = Vec::new();
        for _ in 0..entries_count {
            let mut record = reader.record(2)?;
            let name = record.string(MAX_ID_BYTES)?.to_owned();
            validate_entrypoint_name(&name)?;
            let refs_count = record.count(5, &mut budget)?;
            let mut sets = Vec::new();
            for _ in 0..refs_count {
                sets.push(read_id(&mut record)?);
            }
            record.finish()?;
            if sets.windows(2).any(|pair| pair[0] >= pair[1]) {
                return Err(MetadataError::new("selection: noncanonical set-name order"));
            }
            entrypoints.push(EntrypointSelection { name, sets });
        }
        reader.finish()?;
        budget.finish(expected)?;
        let value = Self { pins, entrypoints };
        value.validate()?;
        if value
            .pins
            .windows(2)
            .any(|pair| pair[0].name >= pair[1].name)
            || value
                .entrypoints
                .windows(2)
                .any(|pair| pair[0].name >= pair[1].name)
        {
            return Err(MetadataError::new(
                "selection table: noncanonical name order",
            ));
        }
        Ok(value)
    }
}

fn validate_entrypoint_name(name: &str) -> Result<(), MetadataError> {
    if name.is_empty() || name.len() > MAX_ID_BYTES || name.contains('\0') {
        return Err(MetadataError::new(
            "entrypoint name: requires 1..=128 UTF-8 bytes without NUL",
        ));
    }
    Ok(())
}

fn write_pin(writer: &mut Writer, pin: &SetPin, tag: u8) -> Result<(), MetadataError> {
    let start = writer.start_record(tag)?;
    writer.string(pin.name.as_str())?;
    writer.bytes(&pin.hash)?;
    writer.end_record(start)
}

fn read_pin(reader: &mut Reader<'_>, tag: u8) -> Result<SetPin, MetadataError> {
    let mut record = reader.record(tag)?;
    let name = read_id(&mut record)?;
    let mut hash = [0; 32];
    hash.copy_from_slice(record.take(32)?);
    record.finish()?;
    Ok(SetPin { name, hash })
}

fn write_path(writer: &mut Writer, path: &RuntimePathGroup) -> Result<(), MetadataError> {
    let start = writer.start_record(3)?;
    writer.string(path.id.as_str())?;
    writer.byte(match path.kind {
        ResourceKind::File => 1,
        ResourceKind::Directory => 2,
    })?;
    writer.byte(match path.selection {
        PathSelection::First => 1,
        PathSelection::All => 2,
        PathSelection::FirstPerLibraryRoot => 3,
    })?;
    writer.byte(match path.inheritance {
        Inheritance::Preserve => 1,
        Inheritance::Replace => 2,
    })?;
    writer.count(path.candidates.len())?;
    for candidate in &path.candidates {
        let candidate_start = writer.start_record(4)?;
        writer.byte(match candidate.base {
            RuntimeBase::PackageRoot => 1,
            RuntimeBase::LibraryRoot => 2,
        })?;
        writer.string(candidate.path.as_str())?;
        writer.end_record(candidate_start)?;
    }
    writer.count(path.variables.len())?;
    for variable in &path.variables {
        writer.string(variable.as_str())?;
    }
    writer.end_record(start)
}

fn read_path(
    reader: &mut Reader<'_>,
    budget: &mut Budget,
) -> Result<RuntimePathGroup, MetadataError> {
    let mut record = reader.record(3)?;
    let id = read_id(&mut record)?;
    let kind = match record.byte()? {
        1 => ResourceKind::File,
        2 => ResourceKind::Directory,
        _ => {
            return Err(MetadataError::new(
                "runtime path: unknown resource kind tag",
            ));
        }
    };
    let selection = match record.byte()? {
        1 => PathSelection::First,
        2 => PathSelection::All,
        3 => PathSelection::FirstPerLibraryRoot,
        _ => return Err(MetadataError::new("runtime path: unknown selection tag")),
    };
    let inheritance = match record.byte()? {
        1 => Inheritance::Preserve,
        2 => Inheritance::Replace,
        _ => return Err(MetadataError::new("runtime path: unknown inheritance tag")),
    };
    let candidates_count = record.count(11, budget)?;
    let mut candidates = Vec::new();
    for _ in 0..candidates_count {
        let mut candidate = record.record(4)?;
        let base = match candidate.byte()? {
            1 => RuntimeBase::PackageRoot,
            2 => RuntimeBase::LibraryRoot,
            _ => return Err(MetadataError::new("runtime candidate: unknown base tag")),
        };
        let encoded_path = candidate.string(MAX_PATH_BYTES)?;
        let path = RelativePath::new(encoded_path)?;
        if path.as_str() != encoded_path {
            return Err(MetadataError::new(
                "runtime candidate: nonnormalized relative path",
            ));
        }
        candidate.finish()?;
        candidates.push(RuntimeCandidate { base, path });
    }
    let variables_count = record.count(5, budget)?;
    let mut variables = Vec::new();
    for _ in 0..variables_count {
        variables.push(EnvironmentName::new(record.string(MAX_ID_BYTES)?)?);
    }
    record.finish()?;
    Ok(RuntimePathGroup {
        id,
        kind,
        candidates,
        variables,
        selection,
        inheritance,
    })
}

fn read_id(reader: &mut Reader<'_>) -> Result<RuleId, MetadataError> {
    Ok(RuleId::new(reader.string(MAX_ID_BYTES)?)?)
}

#[derive(Default)]
struct Budget(usize);

impl Budget {
    fn add(&mut self, count: usize) -> Result<(), MetadataError> {
        self.0 = self
            .0
            .checked_add(count)
            .filter(|count| *count <= MAX_RECORDS)
            .ok_or_else(|| MetadataError::new("records: exceeds MAX_RECORDS (4096)"))?;
        Ok(())
    }

    fn finish(&self, expected: usize) -> Result<(), MetadataError> {
        if self.0 != expected {
            return Err(MetadataError::new(
                "header record count: inconsistent with aggregate records",
            ));
        }
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], MetadataError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| MetadataError::new("length: arithmetic overflow"))?;
        let value = self.bytes.get(self.offset..end).ok_or_else(|| {
            MetadataError::new("metadata: truncated field or length exceeds remaining bytes")
        })?;
        self.offset = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, MetadataError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<usize, MetadataError> {
        let value = self.take(4)?;
        usize::try_from(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
            .map_err(|_| MetadataError::new("length: cannot represent u32 on this platform"))
    }

    fn count(&mut self, minimum_bytes: usize, budget: &mut Budget) -> Result<usize, MetadataError> {
        let count = self.u32()?;
        budget.add(count)?;
        if count > (self.bytes.len() - self.offset) / minimum_bytes {
            return Err(MetadataError::new(
                "record count: exceeds remaining encoded records",
            ));
        }
        Ok(count)
    }

    fn string(&mut self, maximum: usize) -> Result<&'a str, MetadataError> {
        let count = self.u32()?;
        if count == 0 || count > maximum {
            return Err(MetadataError::new(if maximum == MAX_ID_BYTES {
                "name length: requires 1..=128 bytes"
            } else {
                "path length: requires 1..=4096 bytes"
            }));
        }
        std::str::from_utf8(self.take(count)?)
            .map_err(|_| MetadataError::new("string: invalid UTF-8"))
    }

    fn record(&mut self, tag: u8) -> Result<Reader<'a>, MetadataError> {
        let count = self.u32()?;
        if count == 0 {
            return Err(MetadataError::new(
                "record length: must include a known tag",
            ));
        }
        let mut record = Reader {
            bytes: self.take(count)?,
            offset: 0,
        };
        if record.byte()? != tag {
            return Err(MetadataError::new(
                "record: unknown or misplaced variant tag",
            ));
        }
        Ok(record)
    }

    fn finish(&self) -> Result<(), MetadataError> {
        if self.offset != self.bytes.len() {
            return Err(MetadataError::new(
                "metadata: trailing bytes in file or record",
            ));
        }
        Ok(())
    }
}

fn read_header<'a>(bytes: &'a [u8], magic: &[u8; 4]) -> Result<(Reader<'a>, usize), MetadataError> {
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(MetadataError::new(
            "file: exceeds MAX_METADATA_BYTES (1 MiB)",
        ));
    }
    let mut header = Reader { bytes, offset: 0 };
    if header.take(4)? != magic {
        return Err(MetadataError::new("header: invalid metadata magic"));
    }
    let version = header.take(2)?;
    if u16::from_le_bytes([version[0], version[1]]) != METADATA_VERSION {
        return Err(MetadataError::new("header: unsupported metadata version"));
    }
    if header.take(2)? != [0, 0] {
        return Err(MetadataError::new("header: unknown reserved flags"));
    }
    let payload = header.u32()?;
    let records = header.u32()?;
    if records > MAX_RECORDS {
        return Err(MetadataError::new("header: exceeds MAX_RECORDS (4096)"));
    }
    if payload != bytes.len() - HEADER_BYTES {
        return Err(MetadataError::new(
            "header payload length: inconsistent with complete file",
        ));
    }
    Ok((
        Reader {
            bytes: &bytes[HEADER_BYTES..],
            offset: 0,
        },
        records,
    ))
}

struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn new(magic: [u8; 4], records: usize) -> Result<Self, MetadataError> {
        let mut writer = Self { bytes: Vec::new() };
        writer.bytes(&magic)?;
        writer.bytes(&METADATA_VERSION.to_le_bytes())?;
        writer.bytes(&[0, 0])?;
        writer.count(0)?;
        writer.count(records)?;
        Ok(writer)
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<(), MetadataError> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > MAX_METADATA_BYTES)
        {
            return Err(MetadataError::new(
                "file: exceeds MAX_METADATA_BYTES (1 MiB)",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn byte(&mut self, value: u8) -> Result<(), MetadataError> {
        self.bytes(&[value])
    }

    fn count(&mut self, count: usize) -> Result<(), MetadataError> {
        let count = u32::try_from(count).map_err(|_| MetadataError::new("length: exceeds u32"))?;
        self.bytes(&count.to_le_bytes())
    }

    fn string(&mut self, value: &str) -> Result<(), MetadataError> {
        self.count(value.len())?;
        self.bytes(value.as_bytes())
    }

    fn start_record(&mut self, tag: u8) -> Result<usize, MetadataError> {
        let start = self.bytes.len();
        self.count(0)?;
        self.byte(tag)?;
        Ok(start)
    }

    fn end_record(&mut self, start: usize) -> Result<(), MetadataError> {
        let count = u32::try_from(self.bytes.len() - start - 4)
            .map_err(|_| MetadataError::new("record length: exceeds u32"))?;
        self.bytes[start..start + 4].copy_from_slice(&count.to_le_bytes());
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, MetadataError> {
        let count = u32::try_from(self.bytes.len() - HEADER_BYTES)
            .map_err(|_| MetadataError::new("payload length: exceeds u32"))?;
        self.bytes[8..12].copy_from_slice(&count.to_le_bytes());
        Ok(self.bytes)
    }
}
