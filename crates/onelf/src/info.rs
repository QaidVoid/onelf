//! Package metadata inspection.
//!
//! Reads the footer and manifest from a packed ONELF binary and displays
//! format version, layout offsets, entrypoints, and compression statistics.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use onelf_format::runtime_metadata::{EntrypointSets, MetadataError};
use onelf_format::{EntryKind, FOOTER_SIZE, Flags, Footer, HostLibsPolicy, Manifest, elf};

/// Where a package carries its validated runtime rule record.
const RUNTIME_RULES_FILE: &str = ".onelf/runtime-rules";

/// Where a package carries its canonical pin table and entrypoint
/// selections.
const ENTRYPOINT_SETS_FILE: &str = ".onelf/entrypoint-sets";

pub fn info(path: &Path) -> io::Result<()> {
    let (footer, manifest) = read_footer_and_manifest(path)?;

    println!("onelf binary: {}", path.display());
    println!();
    println!("Format version: {}", footer.format_version);
    println!("Flags:          {:?}", footer.flags);
    println!(
        "Host libs:      {}",
        HostLibsPolicy::from_flags(footer.flags).as_str()
    );
    println!(
        "Cache:          {}",
        if footer.flags.contains(Flags::CACHE_PREFERRED) {
            "preferred over every other mode"
        } else if footer.flags.contains(Flags::CACHE_REQUESTED) {
            "requested as last resort"
        } else {
            "only on request"
        }
    );
    println!();
    println!("Manifest:");
    println!("  Offset:       {}", footer.manifest_offset);
    println!("  Compressed:   {} bytes", footer.manifest_compressed);
    println!("  Original:     {} bytes", footer.manifest_original);
    println!(
        "  Checksum:     {:02x}{:02x}{:02x}{:02x}",
        footer.manifest_checksum[0],
        footer.manifest_checksum[1],
        footer.manifest_checksum[2],
        footer.manifest_checksum[3]
    );
    println!();
    println!("Payload:");
    println!(
        "  Format:       {}",
        if footer.is_stipple() {
            "stipplefs image"
        } else {
            "blocks"
        }
    );
    println!("  Offset:       {}", footer.payload_offset);
    println!("  Size:         {} bytes", footer.payload_size);
    println!();

    if footer.dict_size > 0 {
        println!("Dictionary:");
        println!("  Offset:       {}", footer.dict_offset);
        println!("  Size:         {} bytes", footer.dict_size);
        println!();
    }

    let file_count = manifest
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .count();
    let dir_count = manifest
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Dir)
        .count();
    let symlink_count = manifest
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Symlink)
        .count();

    println!("Package ID:     {}", hex(&manifest.header.package_id));
    println!(
        "Entries:        {} ({} dirs, {} files, {} symlinks)",
        manifest.header.entry_count, dir_count, file_count, symlink_count
    );
    println!();

    if let Some(info) = read_package_info(path, &footer, &manifest)? {
        println!("Metadata:");
        for line in info.lines() {
            println!("  {line}");
        }
        println!();
    }

    println!("Provenance:");
    match read_metadata_string(
        path,
        &footer,
        &manifest,
        crate::bundle::sysroot::PROVENANCE_FILE,
    )? {
        None => println!("  none recorded (built from a host scan)"),
        Some(text) => match parse_provenance(&text) {
            Ok((platform, packages)) => {
                println!("  Platform:     {platform}");
                for (name, version) in packages {
                    println!("  {name} {version}");
                }
            }
            Err(e) => println!("  malformed record: {e}"),
        },
    }
    if let Some(text) = read_metadata_string(
        path,
        &footer,
        &manifest,
        crate::bundle::sysroot::PLATFORM_FILE,
    )? {
        println!("  GL build:");
        for line in text.lines() {
            println!("    {line}");
        }
    }
    if let Some(text) =
        read_metadata_string(path, &footer, &manifest, crate::bundle::sysroot::SETS_FILE)?
    {
        println!("  Shared sets:");
        for line in text.lines().filter(|l| !l.is_empty()) {
            println!("    {line}");
        }
    }
    println!();

    let selections = selection_lines(path, &footer, &manifest)?;
    if !selections.is_empty() {
        println!("Sets:");
        for line in selections {
            println!("{line}");
        }
        println!();
    }

    let capabilities = capability_lines(path, &footer, &manifest)?;
    if !capabilities.is_empty() {
        println!("Runtime metadata:");
        for line in capabilities {
            println!("{line}");
        }
        println!();
    }

    if let Some(url) = read_metadata_string(path, &footer, &manifest, ".onelf/update-url")? {
        let unsigned =
            read_metadata_bytes(path, &footer, &manifest, ".onelf/update-unsigned")?.is_some();
        let embedded_key = read_metadata_bytes(path, &footer, &manifest, ".onelf/update-key")?;
        let key_desc = match embedded_key.as_deref().map(|b| b.len() / 32) {
            None if unsigned => "none (updates are not signature-verified)".to_string(),
            None => "none".to_string(),
            Some(n) if n <= 1 => "embedded".to_string(),
            Some(n) => format!("keyring of {n} keys"),
        };
        println!("Update:");
        println!("  URL:          {url}");
        if let Some(rev) = read_metadata_string(path, &footer, &manifest, ".onelf/update-revision")?
        {
            println!("  Revision:     {}", rev.trim());
        }
        println!("  Signing key:  {key_desc}");
        let updater = if footer.flags.contains(Flags::EXTERNAL_UPDATER) {
            "external (this package does not update itself)".to_string()
        } else if unsigned && embedded_key.is_some() {
            "embedded, but refuses to update: a signing key and the \
             unsigned marker contradict each other"
                .to_string()
        } else if unsigned {
            "embedded (installs updates without signature verification)".to_string()
        } else if embedded_key.is_some() {
            "embedded".to_string()
        } else {
            "embedded, but disabled without a signing key".to_string()
        };
        println!("  Updater:      {updater}");
        println!();
    }

    println!("Entrypoints:");
    // A package that anchors directories never runs from a memfd, which
    // keeps no tree for them to name.
    let anchored = (0..manifest.entries.len()).any(|i| {
        manifest.entries[i].kind == EntryKind::File
            && manifest.entry_path(i) == onelf_format::anchors::ANCHORS_FILE
    });
    for (i, ep) in manifest.entrypoints.iter().enumerate() {
        let name = manifest.get_string(ep.name);
        let target_path = manifest.entry_path(ep.target_entry as usize);
        let args = manifest.get_string(ep.args);
        let default_marker = if i == manifest.header.default_entrypoint as usize {
            " (default)"
        } else {
            ""
        };
        let memfd = if ep.is_memfd_eligible() && !anchored {
            " [memfd]"
        } else {
            ""
        };
        print!("  {}{}{}: {}", name, default_marker, memfd, target_path);
        if !args.is_empty() {
            print!(" args={}", args.replace('\x1f', " "));
        }
        println!();
    }

    let total_original: u64 = manifest
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .map(|e| e.blocks.iter().map(|b| b.original_size).sum::<u64>())
        .sum();

    println!();
    println!(
        "Total original size:     {} bytes ({:.1} MB)",
        total_original,
        total_original as f64 / 1_048_576.0
    );
    println!(
        "Total compressed size:   {} bytes ({:.1} MB)",
        footer.payload_size,
        footer.payload_size as f64 / 1_048_576.0
    );
    if total_original > 0 {
        let ratio = footer.payload_size as f64 / total_original as f64 * 100.0;
        println!("Compression ratio:       {:.1}%", ratio);
    }

    Ok(())
}

/// The lines `onelf info` prints under `Sets:`: the declared pin table,
/// each entrypoint's selected sets from the carried selection record,
/// and the provider conflicts the records reveal. Only package records
/// are read; no set artifact is obtained, inspected, or weighed.
pub(crate) fn selection_lines(
    path: &Path,
    footer: &Footer,
    manifest: &Manifest,
) -> io::Result<Vec<String>> {
    let table = read_metadata_string(path, footer, manifest, crate::bundle::sysroot::SETS_FILE)?;
    let record = read_metadata_bytes(path, footer, manifest, ENTRYPOINT_SETS_FILE)?;
    if table.is_none() && record.is_none() {
        return Ok(Vec::new());
    }
    let declared = table
        .as_deref()
        .map(onelf_rt::sets::parse)
        .unwrap_or_default();
    let record = record.as_deref().map(EntrypointSets::decode);
    let mut lines = Vec::new();

    if !declared.is_empty() {
        lines.push("  Declared pins:".to_string());
        for set in &declared {
            lines.push(format!(
                "    {}: pinned {}, from {}",
                set.label, set.blake3, set.url
            ));
        }
    }

    match &record {
        Some(Ok(record)) if !record.entrypoints.is_empty() => {
            lines.push("  Selections:".to_string());
            let default = manifest
                .entrypoints
                .get(manifest.header.default_entrypoint as usize)
                .map(|entry| manifest.get_string(entry.name));
            for entry in &record.entrypoints {
                let marker = if default == Some(entry.name.as_str()) {
                    " (default)"
                } else {
                    ""
                };
                let selected = if entry.sets.is_empty() {
                    "none".to_string()
                } else {
                    entry
                        .sets
                        .iter()
                        .map(|set| set.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                lines.push(format!("    {}{marker}: {selected}", entry.name));
            }
            let selected: BTreeSet<&str> = record
                .entrypoints
                .iter()
                .flat_map(|entry| entry.sets.iter().map(|set| set.as_str()))
                .collect();
            let unselected: Vec<&str> = record
                .pins
                .iter()
                .map(|pin| pin.name.as_str())
                .filter(|name| !selected.contains(name))
                .collect();
            if !unselected.is_empty() {
                lines.push(format!(
                    "  Declared but selected by no entrypoint: {}",
                    unselected.join(", ")
                ));
            }
        }
        Some(Ok(_)) => lines.push("  Selections: the record names no entrypoint".to_string()),
        Some(Err(why)) => lines.push(format!(
            "  Selections: the carried record is unreadable: {why}"
        )),
        None if !declared.is_empty() => {
            lines.push(
                "  Selections: every declared set (the package carries no selection record)"
                    .to_string(),
            );
        }
        None => {}
    }

    let names: Vec<&str> = manifest
        .entrypoints
        .iter()
        .map(|entry| manifest.get_string(entry.name))
        .collect();
    let findings = selection_findings(&declared, record.as_ref(), &names);
    if !findings.is_empty() {
        lines.push("  Findings:".to_string());
        for finding in findings {
            lines.push(crate::bundle::ui::finding_line(&finding));
        }
    }
    Ok(lines)
}

/// The provider conflicts the pin table and the selection record reveal
/// without reading any artifact: a name declared more than once, a
/// selection naming a set the pin table does not declare, and a record
/// that does not match the manifest's entrypoints or the declared pins.
/// Every line is sorted, so the report is a function of the records.
pub(crate) fn selection_findings(
    declared: &[onelf_rt::platform::Pin],
    record: Option<&Result<EntrypointSets, MetadataError>>,
    entrypoint_names: &[&str],
) -> Vec<String> {
    let mut findings = Vec::new();
    let mut seen = BTreeSet::new();
    for set in declared {
        if !seen.insert(set.label.as_str()) {
            findings.push(format!(
                "the set '{}' is declared more than once",
                set.label
            ));
        }
    }
    let Some(Ok(record)) = record else {
        findings.sort();
        return findings;
    };
    let declared: BTreeMap<&str, &str> = declared
        .iter()
        .map(|set| (set.label.as_str(), set.blake3.as_str()))
        .collect();
    for entry in &record.entrypoints {
        for name in &entry.sets {
            if !declared.contains_key(name.as_str()) {
                findings.push(format!(
                    "entrypoint '{}' selects '{}', which the pin table does not declare",
                    entry.name,
                    name.as_str()
                ));
            }
        }
    }
    let mut manifest_names: Vec<&str> = entrypoint_names.to_vec();
    manifest_names.sort_unstable();
    let mut record_names: Vec<&str> = record
        .entrypoints
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    record_names.sort_unstable();
    if manifest_names != record_names {
        findings.push("the selection table does not match the package's entrypoints".to_string());
    }
    let mut record_pins: BTreeMap<&str, String> = BTreeMap::new();
    for pin in &record.pins {
        record_pins.insert(pin.name.as_str(), hex(&pin.hash));
    }
    for (name, hash) in &record_pins {
        match declared.get(name) {
            None => findings.push(format!(
                "the selection record pins '{name}', which the pin table does not declare"
            )),
            Some(pinned) if pinned != hash => {
                findings.push(format!(
                    "the pin for '{name}' differs between the pin table and the selection record"
                ));
            }
            Some(_) => {}
        }
    }
    for name in declared.keys() {
        if !record_pins.contains_key(name) {
            findings.push(format!(
                "the pin table declares '{name}', which the selection record does not carry"
            ));
        }
    }
    findings.sort();
    findings.dedup();
    findings
}

/// The lines `onelf info` prints under `Runtime metadata:` for a package
/// that carries payload metadata records: what they require of a
/// runtime, what the runtime embedded in the package advertises, and
/// whether the two agree. The note is probed in the package's own bytes;
/// nothing outside the package is read or run.
pub(crate) fn capability_lines(
    path: &Path,
    footer: &Footer,
    manifest: &Manifest,
) -> io::Result<Vec<String>> {
    let rules = read_metadata_bytes(path, footer, manifest, RUNTIME_RULES_FILE)?;
    let sets = read_metadata_bytes(path, footer, manifest, ENTRYPOINT_SETS_FILE)?;
    let anchors = read_metadata_bytes(path, footer, manifest, onelf_format::anchors::ANCHORS_FILE)?;
    if rules.is_none() && sets.is_none() && anchors.is_none() {
        return Ok(Vec::new());
    }
    let sets_record = sets.as_deref().map(EntrypointSets::decode);
    let mut required = if rules.is_some() || sets.is_some() {
        elf::RuntimeRequirements::metadata(elf::RUNTIME_METADATA_VERSION)
    } else {
        elf::RuntimeRequirements::NONE
    };
    if let Some(Ok(record)) = &sets_record
        && !record.entrypoints.is_empty()
    {
        required.capabilities = required
            .capabilities
            .union(elf::RuntimeCapabilities::SELECTION_VIEW);
    }
    let anchor_record = anchors.as_deref().map(onelf_format::anchors::decode);
    if let Some(Ok(record)) = &anchor_record
        && !record.is_empty()
    {
        required.capabilities = required
            .capabilities
            .union(elf::RuntimeCapabilities::ANCHORS);
    }

    let mut carried: Vec<&str> = Vec::new();
    if rules.is_some() {
        carried.push("runtime-rules");
    }
    if sets.is_some() {
        carried.push("entrypoint-sets");
    }
    if anchors.is_some() {
        carried.push("anchors");
    }
    let mut lines = vec![
        format!("  Records:     {}", carried.join(", ")),
        format!("  Requires:    {}", requirements_text(&required)),
    ];
    if let Some(Err(why)) = &sets_record {
        lines.push(format!("  Selection table: unreadable: {why}"));
    }
    if let Some(Err(why)) = &anchor_record {
        lines.push(format!("  Anchor record: unreadable: {why}"));
    }
    let mut file = File::open(path)?;
    let mut prefix = vec![0u8; footer.manifest_offset as usize];
    file.read_exact(&mut prefix)?;
    match elf::probe_capability_note(&prefix) {
        Ok(note) => {
            lines.push(format!("  Runtime:     {}", advertisement_text(&note)));
            match elf::check_runtime_support(&note, required) {
                Ok(()) => lines.push("  Agreement:   satisfied".to_string()),
                Err(why) => lines.push(format!("  Agreement:   unsatisfied: {why}")),
            }
        }
        Err(why) => lines.push(format!("  Runtime:     could not be probed: {why}")),
    }
    Ok(lines)
}

/// The support `required` names, as one report line: the metadata
/// schema version, then any named capability.
fn requirements_text(required: &elf::RuntimeRequirements) -> String {
    let mut parts = Vec::new();
    if required.metadata_version != 0 {
        parts.push(format!(
            "payload metadata version {}",
            required.metadata_version
        ));
    }
    if !required.capabilities.is_empty() {
        parts.push(required.capabilities.to_string());
    }
    if parts.is_empty() {
        return "(none)".to_string();
    }
    parts.join(", ")
}

/// What a probed note advertises, as one report line.
fn advertisement_text(note: &elf::RuntimeNote) -> String {
    match note {
        elf::RuntimeNote::Present {
            metadata_version,
            capabilities,
        } => format!("advertises payload metadata version {metadata_version}, {capabilities}"),
        elf::RuntimeNote::Absent => {
            "carries no capability note (a runtime from before payload metadata)".to_string()
        }
        elf::RuntimeNote::Incompatible(why) => {
            format!("carries an unreadable capability note: {why}")
        }
    }
}

/// Read and verify the manifest `footer` describes, from an already
/// bounds-checked file.
fn read_manifest<R: Read + Seek>(reader: &mut R, footer: &Footer) -> io::Result<Manifest> {
    reader.seek(SeekFrom::Start(footer.manifest_offset))?;
    let mut compressed = vec![0u8; footer.manifest_compressed as usize];
    reader.read_exact(&mut compressed)?;

    let bytes =
        zstd::bulk::decompress(&compressed, footer.manifest_original as usize).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("manifest decompression failed: {e}"),
            )
        })?;

    if onelf_format::manifest::manifest_checksum(&bytes)? != footer.manifest_checksum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest checksum mismatch",
        ));
    }
    Manifest::deserialize(&bytes)
}

pub fn read_footer_and_manifest(path: &Path) -> io::Result<(Footer, Manifest)> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();

    if file_size < FOOTER_SIZE as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file too small for onelf footer",
        ));
    }

    file.seek(SeekFrom::End(-(FOOTER_SIZE as i64)))?;
    let mut footer_buf = [0u8; FOOTER_SIZE];
    file.read_exact(&mut footer_buf)?;
    let footer = Footer::from_bytes(&footer_buf)?;

    // Every size below comes from the footer, so nothing may be allocated or
    // seeked to before the regions are checked against the real file.
    onelf_format::reader::validate_footer(&footer, file_size)?;

    let manifest = read_manifest(&mut file, &footer)?;
    Ok((footer, manifest))
}

/// The platform label and `(name, version)` pairs of a provenance record.
/// Display only: nothing the runtime does depends on it.
fn parse_provenance(text: &str) -> Result<(String, Vec<(String, String)>), String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let platform = value
        .get("platform")
        .and_then(toml::Value::as_str)
        .ok_or("no platform label")?
        .to_string();
    let mut packages = Vec::new();
    if let Some(list) = value.get("package") {
        let list = list.as_array().ok_or("package is not a list")?;
        for entry in list {
            let name = entry
                .get("name")
                .and_then(toml::Value::as_str)
                .ok_or("package without a name")?;
            let version = entry
                .get("version")
                .and_then(toml::Value::as_str)
                .ok_or("package without a version")?;
            packages.push((name.to_string(), version.to_string()));
        }
    }
    Ok((platform, packages))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read a small `.onelf/` metadata file as text, if the package has it.
fn read_metadata_string(
    path: &Path,
    footer: &Footer,
    manifest: &Manifest,
    entry_path: &str,
) -> io::Result<Option<String>> {
    Ok(read_metadata_bytes(path, footer, manifest, entry_path)?
        .map(|data| String::from_utf8_lossy(&data).trim().to_string()))
}

/// The raw bytes of a metadata entry, without the UTF-8 decoding
/// `read_metadata_string` applies. Binary metadata (the update keyring)
/// needs the exact length.
fn read_metadata_bytes(
    path: &Path,
    footer: &Footer,
    manifest: &Manifest,
    entry_path: &str,
) -> io::Result<Option<Vec<u8>>> {
    use crate::extract::decompress_verified;

    let idx = (0..manifest.entries.len()).find(|&i| {
        manifest.entries[i].kind == EntryKind::File && manifest.entry_path(i) == entry_path
    });
    let Some(idx) = idx else { return Ok(None) };

    let mut file = File::open(path)?;
    let codec = onelf_format::payload::Codec::open(&file, footer)?;
    Ok(Some(decompress_verified(
        &mut file,
        footer,
        &manifest.entries[idx],
        &codec,
    )?))
}

fn read_package_info(
    path: &Path,
    footer: &Footer,
    manifest: &Manifest,
) -> io::Result<Option<String>> {
    use crate::extract::decompress_verified;

    let idx = (0..manifest.entries.len()).find(|&i| {
        manifest.entries[i].kind == EntryKind::File
            && manifest.entry_path(i) == ".onelf/package-info.toml"
    });
    let Some(idx) = idx else { return Ok(None) };

    // The footer is already parsed by the caller; just reopen for the payload.
    let mut file = File::open(path)?;
    let codec = onelf_format::payload::Codec::open(&file, footer)?;

    let data = decompress_verified(&mut file, footer, &manifest.entries[idx], &codec)?;
    let text = String::from_utf8(data).unwrap_or_default();
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_requirement_reads_none() {
        assert_eq!(requirements_text(&elf::RuntimeRequirements::NONE), "(none)");
    }
}
