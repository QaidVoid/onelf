//! File listing for packed ONELF binaries.
//!
//! Reads the manifest and prints a tabular listing of all entries with
//! their mode, type, sizes, content hash, and path.

use std::io;
use std::path::Path;

use onelf_format::EntryKind;

use crate::info::read_footer_and_manifest;

pub fn list(path: &Path) -> io::Result<()> {
    let (_footer, manifest) = read_footer_and_manifest(path)?;

    println!(
        "{:<10} {:<7} {:<12} {:<12} {:<66} {}",
        "MODE", "TYPE", "ORIGINAL", "COMPRESSED", "HASH", "PATH"
    );
    println!("{}", "-".repeat(120));

    for (i, entry) in manifest.entries.iter().enumerate() {
        let path = manifest.entry_path(i);
        if path.is_empty() {
            continue; // skip root
        }

        let kind = match entry.kind {
            EntryKind::Dir => "dir",
            EntryKind::File => "file",
            EntryKind::Symlink => "link",
        };

        let mode = format!("{:o}", entry.mode & 0o7777);

        match entry.kind {
            EntryKind::File => {
                let total_original: u64 = entry.blocks.iter().map(|b| b.original_size).sum();
                let total_compressed: u64 = entry.blocks.iter().map(|b| b.compressed_size).sum();
                let hash: String = entry
                    .content_hash
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                println!(
                    "{:<10} {:<7} {:<12} {:<12} {} {}",
                    mode, kind, total_original, total_compressed, hash, path
                );
            }
            EntryKind::Symlink => {
                let target = manifest.get_string(entry.symlink_target);
                println!(
                    "{:<10} {:<7} {:<12} {:<12} {:<66} {} -> {}",
                    mode, kind, "-", "-", "-", path, target
                );
            }
            EntryKind::Dir => {
                println!(
                    "{:<10} {:<7} {:<12} {:<12} {:<66} {}",
                    mode, kind, "-", "-", "-", path
                );
            }
        }
    }

    Ok(())
}
