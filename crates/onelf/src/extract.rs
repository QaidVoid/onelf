//! Extraction of files from packed ONELF binaries.
//!
//! Supports three modes:
//! - Full extraction: extracts all entries to an output directory
//! - Selective extraction: extracts specific files by path
//! - Stdout extraction: pipes a single file to stdout (`-o -`)

use std::fs::{self, File};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use indicatif::{ProgressBar, ProgressStyle};
use onelf_format::{EntryKind, symlink_target_within_root};

use crate::info::read_footer_and_manifest;

/// Mask attacker-controlled mode bits unless the caller opts in to
/// preserving the full mode (setuid/setgid/sticky). Off by default.
fn mode_bits(mode: u32, preserve_mode: bool) -> u32 {
    if preserve_mode { mode } else { mode & 0o777 }
}

pub fn extract(
    binary: &Path,
    output: Option<&Path>,
    files: &[String],
    preserve_mode: bool,
) -> io::Result<()> {
    if files.is_empty() {
        let output_dir = output.unwrap_or(Path::new("onelf_extracted"));
        return extract_all(binary, output_dir, preserve_mode);
    }

    extract_selective(binary, output, files, preserve_mode)
}

pub(crate) fn decompress_entry(
    file: &mut File,
    footer: &onelf_format::Footer,
    entry: &onelf_format::Entry,
    dict: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();

    for block in &entry.blocks {
        file.seek(SeekFrom::Start(
            footer.payload_offset + block.payload_offset,
        ))?;
        let mut compressed = vec![0u8; block.compressed_size as usize];
        file.read_exact(&mut compressed)?;

        // Store mode: bytes are the file content verbatim, no zstd.
        if footer.is_stored() {
            result.extend_from_slice(&compressed);
            continue;
        }

        let decompressed = if let Some(d) = dict {
            let cursor = Cursor::new(&compressed);
            let mut decoder = zstd::Decoder::with_dictionary(cursor, d)?;
            let mut block_result = Vec::with_capacity(block.original_size as usize);
            decoder.read_to_end(&mut block_result)?;
            block_result
        } else {
            zstd::bulk::decompress(&compressed, block.original_size as usize).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("decompression failed: {e}"),
                )
            })?
        };

        result.extend_from_slice(&decompressed);
    }

    Ok(result)
}

/// Decompress an entry and verify its bytes against the recorded BLAKE3
/// `content_hash` before returning, so extraction never writes tampered
/// or corrupt content to disk or stdout. `decompress_entry` itself stays
/// unverified because `verify` needs to decompress-then-report every
/// mismatch rather than fail fast.
fn decompress_verified(
    file: &mut File,
    footer: &onelf_format::Footer,
    entry: &onelf_format::Entry,
    dict: Option<&[u8]>,
) -> io::Result<Vec<u8>> {
    let data = decompress_entry(file, footer, entry, dict)?;
    if blake3::hash(&data).as_bytes() != &entry.content_hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "content hash mismatch (tampered or corrupt package)",
        ));
    }
    Ok(data)
}

fn extract_selective(
    binary: &Path,
    output: Option<&Path>,
    files: &[String],
    preserve_mode: bool,
) -> io::Result<()> {
    let (footer, manifest) = read_footer_and_manifest(binary)?;
    let mut file = File::open(binary)?;

    let dict = if footer.dict_size > 0 {
        file.seek(SeekFrom::Start(footer.dict_offset))?;
        let mut dict_buf = vec![0u8; footer.dict_size as usize];
        file.read_exact(&mut dict_buf)?;
        Some(dict_buf)
    } else {
        None
    };

    // Find matching entries
    let matched: Vec<(usize, String)> = manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EntryKind::File)
        .filter_map(|(i, _)| {
            let path = manifest.entry_path(i);
            if files.iter().any(|f| f == &path) {
                Some((i, path))
            } else {
                None
            }
        })
        .collect();

    if matched.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no files matched: {}", files.join(", ")),
        ));
    }

    let to_stdout = output.map_or(false, |p| p.as_os_str() == "-");

    // Single file to stdout
    if to_stdout {
        if matched.len() > 1 || files.len() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdout output (-o -) only supports a single --file",
            ));
        }
        let (idx, _) = &matched[0];
        let entry = &manifest.entries[*idx];
        let data = decompress_verified(&mut file, &footer, entry, dict.as_deref())?;
        io::stdout().write_all(&data)?;
        return Ok(());
    }

    // Single file to a file path (not a directory)
    if matched.len() == 1 && files.len() == 1 {
        let (idx, _) = &matched[0];
        let entry = &manifest.entries[*idx];
        let data = decompress_verified(&mut file, &footer, entry, dict.as_deref())?;

        if let Some(out) = output {
            if out.is_dir() {
                // Output is existing directory — extract preserving relative path
                let rel_path = manifest.validated_entry_path(*idx)?;
                let target = out.join(&rel_path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, &data)?;
                fs::set_permissions(
                    &target,
                    fs::Permissions::from_mode(mode_bits(entry.mode, preserve_mode)),
                )?;
            } else {
                // Output is a file path — write directly
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(out, &data)?;
                fs::set_permissions(
                    out,
                    fs::Permissions::from_mode(mode_bits(entry.mode, preserve_mode)),
                )?;
            }
        } else {
            // No output specified — extract to current dir preserving relative path
            let target = manifest.validated_entry_path(*idx)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, &data)?;
            fs::set_permissions(
                &target,
                fs::Permissions::from_mode(mode_bits(entry.mode, preserve_mode)),
            )?;
        }
        return Ok(());
    }

    // Multiple files — extract to directory preserving relative paths
    let output_dir = output.unwrap_or(Path::new("onelf_extracted"));
    fs::create_dir_all(output_dir)?;

    for (idx, _) in &matched {
        let entry = &manifest.entries[*idx];
        let data = decompress_verified(&mut file, &footer, entry, dict.as_deref())?;
        let rel_path = manifest.validated_entry_path(*idx)?;
        let target = output_dir.join(&rel_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, &data)?;
        fs::set_permissions(
            &target,
            fs::Permissions::from_mode(mode_bits(entry.mode, preserve_mode)),
        )?;
    }

    Ok(())
}

fn extract_all(binary: &Path, output_dir: &Path, preserve_mode: bool) -> io::Result<()> {
    let (footer, manifest) = read_footer_and_manifest(binary)?;

    let mut file = File::open(binary)?;

    // Read dictionary if present
    let dict = if footer.dict_size > 0 {
        file.seek(SeekFrom::Start(footer.dict_offset))?;
        let mut dict_buf = vec![0u8; footer.dict_size as usize];
        file.read_exact(&mut dict_buf)?;
        Some(dict_buf)
    } else {
        None
    };

    let file_count = manifest
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .count();
    let pb = ProgressBar::new(file_count as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=> "),
    );
    pb.set_message("Extracting...");

    fs::create_dir_all(output_dir)?;

    // First pass: dirs and files (before symlinks, so no file is ever
    // written through a symlink pointing outside the tree).
    for (i, entry) in manifest.entries.iter().enumerate() {
        let rel_path = manifest.validated_entry_path(i)?;
        if rel_path.as_os_str().is_empty() {
            continue;
        }
        let target = output_dir.join(&rel_path);

        match entry.kind {
            EntryKind::Dir => {
                fs::create_dir_all(&target)?;
                fs::set_permissions(
                    &target,
                    fs::Permissions::from_mode(mode_bits(entry.mode, preserve_mode)),
                )?;
            }
            EntryKind::File => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }

                let data = decompress_verified(&mut file, &footer, entry, dict.as_deref())?;

                fs::write(&target, &data)?;
                fs::set_permissions(
                    &target,
                    fs::Permissions::from_mode(mode_bits(entry.mode, preserve_mode)),
                )?;
                pb.inc(1);
            }
            EntryKind::Symlink => {}
        }
    }

    // Second pass: symlinks last; refuse any target that escapes the tree.
    for (i, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != EntryKind::Symlink {
            continue;
        }
        let rel_path = manifest.validated_entry_path(i)?;
        if rel_path.as_os_str().is_empty() {
            continue;
        }
        let link_target = manifest.get_string(entry.symlink_target);
        if !symlink_target_within_root(&rel_path, link_target) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("symlink target escapes output dir: {}", rel_path.display()),
            ));
        }
        let target = output_dir.join(&rel_path);
        if target.symlink_metadata().is_ok() {
            fs::remove_file(&target)?;
        }
        std::os::unix::fs::symlink(link_target, &target)?;
    }

    pb.finish_with_message("Extraction complete");
    Ok(())
}
