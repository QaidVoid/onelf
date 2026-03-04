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
use onelf_format::EntryKind;

use crate::info::read_footer_and_manifest;

pub fn extract(binary: &Path, output: Option<&Path>, files: &[String]) -> io::Result<()> {
    if files.is_empty() {
        let output_dir = output.unwrap_or(Path::new("onelf_extracted"));
        return extract_all(binary, output_dir);
    }

    extract_selective(binary, output, files)
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

fn extract_selective(binary: &Path, output: Option<&Path>, files: &[String]) -> io::Result<()> {
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
        let data = decompress_entry(&mut file, &footer, entry, dict.as_deref())?;
        io::stdout().write_all(&data)?;
        return Ok(());
    }

    // Single file to a file path (not a directory)
    if matched.len() == 1 && files.len() == 1 {
        let (idx, _) = &matched[0];
        let entry = &manifest.entries[*idx];
        let data = decompress_entry(&mut file, &footer, entry, dict.as_deref())?;

        if let Some(out) = output {
            if out.is_dir() {
                // Output is existing directory — extract preserving relative path
                let rel_path = &matched[0].1;
                let target = out.join(rel_path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, &data)?;
                fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode))?;
            } else {
                // Output is a file path — write directly
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(out, &data)?;
                fs::set_permissions(out, fs::Permissions::from_mode(entry.mode))?;
            }
        } else {
            // No output specified — extract to current dir preserving relative path
            let rel_path = &matched[0].1;
            let target = Path::new(rel_path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(target, &data)?;
            fs::set_permissions(target, fs::Permissions::from_mode(entry.mode))?;
        }
        return Ok(());
    }

    // Multiple files — extract to directory preserving relative paths
    let output_dir = output.unwrap_or(Path::new("onelf_extracted"));
    fs::create_dir_all(output_dir)?;

    for (idx, rel_path) in &matched {
        let entry = &manifest.entries[*idx];
        let data = decompress_entry(&mut file, &footer, entry, dict.as_deref())?;
        let target = output_dir.join(rel_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, &data)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode))?;
    }

    Ok(())
}

fn extract_all(binary: &Path, output_dir: &Path) -> io::Result<()> {
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

    for (i, entry) in manifest.entries.iter().enumerate() {
        let rel_path = manifest.entry_path(i);
        if rel_path.is_empty() {
            continue;
        }

        let target = output_dir.join(&rel_path);

        match entry.kind {
            EntryKind::Dir => {
                fs::create_dir_all(&target)?;
                fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode))?;
            }
            EntryKind::File => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }

                let data = decompress_entry(&mut file, &footer, entry, dict.as_deref())?;

                fs::write(&target, &data)?;
                fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode))?;
                pb.inc(1);
            }
            EntryKind::Symlink => {
                let link_target = manifest.get_string(entry.symlink_target);
                if target.exists() || target.symlink_metadata().is_ok() {
                    fs::remove_file(&target)?;
                }
                std::os::unix::fs::symlink(link_target, &target)?;
            }
        }
    }

    pb.finish_with_message("Extraction complete");
    Ok(())
}
