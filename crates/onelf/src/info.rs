//! Package metadata inspection.
//!
//! Reads the footer and manifest from a packed ONELF binary and displays
//! format version, layout offsets, entrypoints, and compression statistics.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use onelf_format::{EntryKind, FOOTER_SIZE, Footer, Manifest};

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

pub fn read_footer_and_manifest(path: &Path) -> io::Result<(Footer, Manifest)> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();

    if file_size < FOOTER_SIZE as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file too small for onelf footer",
        ));
    }

    // Read footer from last 80 bytes
    file.seek(SeekFrom::End(-(FOOTER_SIZE as i64)))?;
    let mut footer_buf = [0u8; FOOTER_SIZE];
    file.read_exact(&mut footer_buf)?;
    let footer = Footer::from_bytes(&footer_buf)?;

    // Read and decompress manifest
    file.seek(SeekFrom::Start(footer.manifest_offset))?;
    let mut manifest_compressed = vec![0u8; footer.manifest_compressed as usize];
    file.read_exact(&mut manifest_compressed)?;

    let manifest_bytes =
        zstd::bulk::decompress(&manifest_compressed, footer.manifest_original as usize).map_err(
            |e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("manifest decompression failed: {e}"),
                )
            },
        )?;

    // Verify checksum
    let checksum = xxhash_rust::xxh32::xxh32(&manifest_bytes, 0).to_le_bytes();
    if checksum != footer.manifest_checksum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest checksum mismatch",
        ));
    }

    let manifest = Manifest::deserialize(&manifest_bytes)?;

    Ok((footer, manifest))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
