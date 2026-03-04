//! Extract icons and desktop files from packed ONELF binaries.
//!
//! Follows the `.onelf/` metadata convention:
//! - Icons resolved as `{name}.svg`, `{name}.png`, `default.svg`, `default.png`
//! - Desktop files resolved as `{name}.desktop`, `default.desktop`

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use onelf_format::{EntryKind, Manifest};

use crate::extract::decompress_entry;
use crate::info::read_footer_and_manifest;

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

fn extract_metadata(
    binary: &Path,
    entrypoint: Option<&str>,
    output: Option<&Path>,
    resolve: fn(&Manifest, &str) -> Option<usize>,
    kind: &str,
) -> io::Result<()> {
    let (footer, manifest) = read_footer_and_manifest(binary)?;

    let ep_name = match entrypoint {
        Some(name) => name.to_string(),
        None => {
            let default_idx = manifest.header.default_entrypoint as usize;
            let ep = &manifest.entrypoints[default_idx];
            manifest.get_string(ep.name).to_string()
        }
    };

    let entry_idx = resolve(&manifest, &ep_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no {kind} found for entrypoint '{ep_name}'"),
        )
    })?;

    let entry = &manifest.entries[entry_idx];
    let mut file = File::open(binary)?;

    let dict = if footer.dict_size > 0 {
        file.seek(SeekFrom::Start(footer.dict_offset))?;
        let mut dict_buf = vec![0u8; footer.dict_size as usize];
        io::Read::read_exact(&mut file, &mut dict_buf)?;
        Some(dict_buf)
    } else {
        None
    };

    let data = decompress_entry(&mut file, &footer, entry, dict.as_deref())?;

    match output {
        Some(path) if path.as_os_str() == "-" => {
            io::stdout().write_all(&data)?;
        }
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, &data)?;
        }
        None => {
            io::stdout().write_all(&data)?;
        }
    }

    Ok(())
}

pub fn icon(binary: &Path, entrypoint: Option<&str>, output: Option<&Path>) -> io::Result<()> {
    extract_metadata(binary, entrypoint, output, resolve_icon, "icon")
}

pub fn desktop(binary: &Path, entrypoint: Option<&str>, output: Option<&Path>) -> io::Result<()> {
    extract_metadata(binary, entrypoint, output, resolve_desktop, "desktop file")
}
