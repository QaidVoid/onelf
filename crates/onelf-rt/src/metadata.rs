//! Self-extraction of icons and desktop files from the running binary.
//!
//! Handles `--onelf-icon` and `--onelf-desktop` flags, resolving metadata
//! from the `.onelf/` convention and writing to stdout.

use std::io::{self, Write};

use onelf_format::{EntryKind, Manifest};

use crate::loader::PackageData;

fn find_entry_by_path(manifest: &Manifest, path: &str) -> Option<usize> {
    manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EntryKind::File)
        .find(|(i, _)| manifest.entry_path(*i) == path)
        .map(|(i, _)| i)
}

fn resolve_icon(manifest: &Manifest, entrypoint: &str) -> Option<usize> {
    let candidates = [
        format!(".onelf/icons/{entrypoint}.svg"),
        format!(".onelf/icons/{entrypoint}.png"),
        ".onelf/icons/default.svg".to_string(),
        ".onelf/icons/default.png".to_string(),
    ];
    candidates
        .iter()
        .find_map(|path| find_entry_by_path(manifest, path))
}

fn resolve_desktop(manifest: &Manifest, entrypoint: &str) -> Option<usize> {
    let candidates = [
        format!(".onelf/desktop/{entrypoint}.desktop"),
        ".onelf/desktop/default.desktop".to_string(),
    ];
    candidates
        .iter()
        .find_map(|path| find_entry_by_path(manifest, path))
}

/// Check args for `--onelf-icon` or `--onelf-desktop` and handle them.
/// Returns `true` if a metadata flag was handled (caller should exit).
pub fn handle_metadata_flags(args: &[String], pkg: &mut PackageData, ep_name: &str) -> bool {
    let is_icon = args.iter().any(|a| a == "--onelf-icon");
    let is_desktop = args.iter().any(|a| a == "--onelf-desktop");

    if !is_icon && !is_desktop {
        return false;
    }

    let (entry_idx, kind) = if is_icon {
        (resolve_icon(&pkg.manifest, ep_name), "icon")
    } else {
        (resolve_desktop(&pkg.manifest, ep_name), "desktop file")
    };

    let Some(idx) = entry_idx else {
        eprintln!("onelf-rt: no {kind} found for entrypoint '{ep_name}'");
        std::process::exit(1);
    };

    let entry = &pkg.manifest.entries[idx];
    match crate::loader::read_payload_blocks(
        &mut pkg.file,
        pkg.footer.payload_offset,
        &entry.blocks,
        pkg.dict.as_deref(),
    ) {
        Ok(data) => {
            if let Err(e) = io::stdout().write_all(&data) {
                eprintln!("onelf-rt: write failed: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("onelf-rt: failed to read {kind}: {e}");
            std::process::exit(1);
        }
    }

    true
}
