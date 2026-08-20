//! Package metadata inspection.
//!
//! Reads the footer and manifest from a packed ONELF binary and displays
//! format version, layout offsets, entrypoints, and compression statistics.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use onelf_format::{EntryKind, FOOTER_SIZE, Flags, Footer, Manifest};

pub fn info(path: &Path) -> io::Result<()> {
    let (footer, manifest) = read_footer_and_manifest(path)?;

    println!("onelf binary: {}", path.display());
    println!();
    println!("Format version: {}", footer.format_version);
    println!("Flags:          {:?}", footer.flags);
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

    if let Some(url) = read_metadata_string(path, &footer, &manifest, ".onelf/update-url")? {
        let has_key =
            read_metadata_string(path, &footer, &manifest, ".onelf/update-key")?.is_some();
        println!("Update:");
        println!("  URL:          {url}");
        println!(
            "  Signing key:  {}",
            if has_key { "embedded" } else { "none" }
        );
        println!(
            "  Updater:      {}",
            if footer.flags.contains(Flags::EXTERNAL_UPDATER) {
                "external (this package does not update itself)"
            } else if has_key {
                "embedded"
            } else {
                "embedded, but disabled without a signing key"
            }
        );
        println!();
    }

    println!("Entrypoints:");
    for (i, ep) in manifest.entrypoints.iter().enumerate() {
        let name = manifest.get_string(ep.name);
        let target_path = manifest.entry_path(ep.target_entry as usize);
        let args = manifest.get_string(ep.args);
        let default_marker = if i == manifest.header.default_entrypoint as usize {
            " (default)"
        } else {
            ""
        };
        let memfd = if ep.is_memfd_eligible() {
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

/// Read the optional zstd dictionary described by `footer` from `reader`,
/// or `None` when the package carries no dictionary. Shared by every command
/// that decompresses payload entries.
pub fn read_dict<R: Read + Seek>(reader: &mut R, footer: &Footer) -> io::Result<Option<Vec<u8>>> {
    if footer.dict_size == 0 {
        return Ok(None);
    }
    reader.seek(SeekFrom::Start(footer.dict_offset))?;
    let mut buf = vec![0u8; footer.dict_size as usize];
    reader.read_exact(&mut buf)?;
    Ok(Some(buf))
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

    if xxhash_rust::xxh32::xxh32(&bytes, 0).to_le_bytes() != footer.manifest_checksum {
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
    use crate::extract::decompress_entry;

    let idx = (0..manifest.entries.len()).find(|&i| {
        manifest.entries[i].kind == EntryKind::File && manifest.entry_path(i) == entry_path
    });
    let Some(idx) = idx else { return Ok(None) };

    let mut file = File::open(path)?;
    let dict = read_dict(&mut file, footer)?;
    let data = decompress_entry(&mut file, footer, &manifest.entries[idx], dict.as_deref())?;
    Ok(Some(String::from_utf8_lossy(&data).trim().to_string()))
}

fn read_package_info(
    path: &Path,
    footer: &Footer,
    manifest: &Manifest,
) -> io::Result<Option<String>> {
    use crate::extract::decompress_entry;

    let idx = (0..manifest.entries.len()).find(|&i| {
        manifest.entries[i].kind == EntryKind::File
            && manifest.entry_path(i) == ".onelf/package-info.toml"
    });
    let Some(idx) = idx else { return Ok(None) };

    // The footer is already parsed by the caller; just reopen for the payload.
    let mut file = File::open(path)?;
    let dict = read_dict(&mut file, footer)?;

    let data = decompress_entry(&mut file, footer, &manifest.entries[idx], dict.as_deref())?;
    let text = String::from_utf8(data).unwrap_or_default();
    Ok(Some(text))
}
