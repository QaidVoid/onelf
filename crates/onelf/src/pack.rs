//! Packs a directory into a self-extracting ONELF binary.
//!
//! The packing process:
//! 1. Scans the source directory for files, directories, and symlinks
//! 2. Optionally trains a zstd dictionary from the collected file contents
//! 3. Compresses files in parallel using zstd (with optional dictionary)
//! 4. Builds a string table, filesystem entries, and entrypoints
//! 5. Serializes the manifest and computes the package ID (BLAKE3)
//! 6. Writes the final binary: `[runtime ELF][manifest][payload][dict?][footer]`

use crate::bundle::format_size;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use indicatif::{ProgressBar, ProgressStyle};
use jwalk::WalkDir;
use rayon::prelude::*;

use onelf_format::{
    Entry, EntryKind, EntryPoint, EntryPointFlags, Flags, Footer, Manifest, ManifestHeader,
    StringTableBuilder, WorkingDir,
};

use crate::compress;

pub struct PackOptions {
    pub directory: PathBuf,
    pub output: PathBuf,
    pub command: String,
    pub name: Option<String>,
    pub entrypoints: Vec<(String, String)>,
    pub default_entrypoint: Option<String>,
    pub lib_dirs: Vec<String>,
    pub level: i32,
    pub use_dict: bool,
    pub memfd: Option<bool>,
    pub working_dir: WorkingDir,
    pub update_url: Option<String>,
    pub exclude: Vec<String>,
}

struct CollectedFile {
    rel_path: PathBuf,
    content: Vec<u8>,
    mode: u32,
    mtime_secs: u64,
    mtime_nsec: u32,
}

struct CompressedFile {
    rel_path: PathBuf,
    blocks: Vec<compress::CompressedBlock>,
    content_hash: [u8; 32],
    mode: u32,
    mtime_secs: u64,
    mtime_nsec: u32,
}

struct CollectedDir {
    rel_path: PathBuf,
    mode: u32,
    mtime_secs: u64,
    mtime_nsec: u32,
}

struct CollectedSymlink {
    rel_path: PathBuf,
    target: PathBuf,
    mode: u32,
    mtime_secs: u64,
    mtime_nsec: u32,
}

fn auto_detect_lib_dirs(directory: &Path) -> Vec<String> {
    let mut lib_dirs = Vec::new();
    let Ok(canonical) = directory.canonicalize() else {
        return lib_dirs;
    };

    for entry in WalkDir::new(&canonical).skip_hidden(false).sort(true) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.contains(".so") {
            continue;
        }

        let rel = path
            .parent()
            .and_then(|p| p.strip_prefix(&canonical).ok())
            .map(|p| p.to_string_lossy().to_string());

        if let Some(dir) = rel {
            if dir.is_empty() {
                continue;
            }
            if dir.starts_with(".onelf")
                || dir.starts_with("share/")
                || dir.starts_with("bin/")
                || dir.starts_with("etc/")
            {
                continue;
            }
            if !lib_dirs.contains(&dir) {
                lib_dirs.push(dir);
            }
        }
    }
    lib_dirs
}

fn check_libgl_conflicts(directory: &Path, lib_dirs: &mut Vec<String>) {
    let mut glvnd_dirs = Vec::new();
    let mut legacy_dirs = Vec::new();

    for dir in lib_dirs.iter() {
        let full = directory.join(dir);
        let gl_path = full.join("libGL.so.1");
        if !gl_path.exists() {
            continue;
        }

        let resolved = std::fs::canonicalize(&gl_path).unwrap_or(gl_path.clone());
        if let Ok(data) = std::fs::read(&resolved) {
            let has_gldispatch = data.windows(14).any(|w| w == b"libGLdispatch\0")
                || data.windows(7).any(|w| w == b"libGLX\0");
            if has_gldispatch {
                glvnd_dirs.push(dir.clone());
            } else {
                legacy_dirs.push(dir.clone());
            }
        }
    }

    if !glvnd_dirs.is_empty() && !legacy_dirs.is_empty() {
        eprintln!("  warning: conflicting libGL.so detected");
        eprintln!("    glvnd (modern): {}", glvnd_dirs.join(", "));
        eprintln!("    legacy Mesa: {}", legacy_dirs.join(", "));
        eprintln!("    Removing legacy dirs to avoid GL initialization failures");
        lib_dirs.retain(|d| !legacy_dirs.contains(d));
    }
}

pub fn pack(opts: &PackOptions, runtime_binary: &[u8]) -> io::Result<()> {
    let dir = opts.directory.canonicalize()?;

    // Resolve lib dirs: handle "auto" detection and conflict checking
    let mut lib_dirs = if opts.lib_dirs.iter().any(|d| d == "auto") {
        let mut dirs: Vec<String> = opts
            .lib_dirs
            .iter()
            .filter(|d| *d != "auto")
            .cloned()
            .collect();
        let auto = auto_detect_lib_dirs(&dir);
        for d in auto {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        if !dirs.is_empty() {
            eprintln!("  Auto-detected lib dirs: {}", dirs.join(", "));
        }
        dirs
    } else {
        opts.lib_dirs.clone()
    };

    check_libgl_conflicts(&dir, &mut lib_dirs);

    // Collect all filesystem entries
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap(),
    );
    pb.set_message("Scanning directory...");

    let mut dirs: Vec<CollectedDir> = Vec::new();
    let mut files: Vec<CollectedFile> = Vec::new();
    let mut symlinks: Vec<CollectedSymlink> = Vec::new();

    for entry in WalkDir::new(&dir).skip_hidden(false).sort(true) {
        let entry = entry.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let abs_path = entry.path();
        let rel_path = abs_path.strip_prefix(&dir).unwrap().to_path_buf();

        if rel_path.as_os_str().is_empty() {
            continue;
        }

        // Check exclude patterns against each path component and file extension
        if !opts.exclude.is_empty() {
            let excluded = rel_path.components().any(|c| {
                let name = c.as_os_str().to_string_lossy();
                opts.exclude.iter().any(|pat| {
                    if let Some(ext) = pat.strip_prefix("*.") {
                        name.ends_with(&format!(".{ext}"))
                    } else {
                        name == pat.as_str()
                    }
                })
            });
            if excluded {
                continue;
            }
        }

        let symlink_meta = fs::symlink_metadata(&abs_path)?;
        let (mtime_secs, mtime_nsec) = get_mtime(&symlink_meta);
        let mode = symlink_meta.permissions().mode();

        if symlink_meta.is_symlink() {
            let target = fs::read_link(&abs_path)?;
            symlinks.push(CollectedSymlink {
                rel_path,
                target,
                mode,
                mtime_secs,
                mtime_nsec,
            });
        } else if symlink_meta.is_dir() {
            dirs.push(CollectedDir {
                rel_path,
                mode,
                mtime_secs,
                mtime_nsec,
            });
        } else if symlink_meta.is_file() {
            let content = fs::read(&abs_path)?;
            files.push(CollectedFile {
                rel_path,
                content,
                mode,
                mtime_secs,
                mtime_nsec,
            });
        }
    }

    // Inject .onelf/update-url if requested
    if let Some(ref url) = opts.update_url {
        if !dirs.iter().any(|d| d.rel_path == Path::new(".onelf")) {
            dirs.push(CollectedDir {
                rel_path: PathBuf::from(".onelf"),
                mode: 0o755,
                mtime_secs: 0,
                mtime_nsec: 0,
            });
        }
        files.push(CollectedFile {
            rel_path: PathBuf::from(".onelf/update-url"),
            content: url.as_bytes().to_vec(),
            mode: 0o644,
            mtime_secs: 0,
            mtime_nsec: 0,
        });
    }

    // Patch ELF PT_INTERP for cross-libc portability.
    // Scans collected files for ELF binaries with PT_INTERP and checks if the
    // corresponding interpreter is bundled in the package (e.g. via bundle-libs).
    let package_name = opts
        .name
        .as_deref()
        .unwrap_or_else(|| opts.command.split('/').last().unwrap_or("app"));

    {
        // Find first PT_INTERP from any ELF file
        let original_interp = files.iter().find_map(|f| elf_interp(&f.content));

        if let Some(ref interp) = original_interp {
            let interp_name = Path::new(interp).file_name().and_then(|n| n.to_str());

            // Find the bundled interpreter among collected files by filename match
            let bundled_relpath = interp_name.and_then(|name| {
                files.iter().find_map(|f| {
                    if f.rel_path.file_name().and_then(|n| n.to_str()) == Some(name) {
                        Some(f.rel_path.to_string_lossy().into_owned())
                    } else {
                        None
                    }
                })
            });

            if let Some(ref bundled_rel) = bundled_relpath {
                let name_hash = blake3::hash(package_name.as_bytes());
                let hash_hex: String = name_hash.as_bytes()[..4]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                let new_interp = format!("/tmp/.oi{hash_hex}");

                let mut patched_count = 0usize;
                for f in files.iter_mut() {
                    if patch_elf_interp(&mut f.content, &new_interp) {
                        patched_count += 1;
                    }
                }

                if patched_count > 0 {
                    eprintln!("Patched {patched_count} ELF binaries: {interp} → {new_interp}");

                    // Inject .onelf/interp metadata for runtime symlink creation
                    if !dirs.iter().any(|d| d.rel_path == Path::new(".onelf")) {
                        dirs.push(CollectedDir {
                            rel_path: PathBuf::from(".onelf"),
                            mode: 0o755,
                            mtime_secs: 0,
                            mtime_nsec: 0,
                        });
                    }
                    files.push(CollectedFile {
                        rel_path: PathBuf::from(".onelf/interp"),
                        content: format!("{interp}\n{new_interp}\n{bundled_rel}").into_bytes(),
                        mode: 0o644,
                        mtime_secs: 0,
                        mtime_nsec: 0,
                    });
                }
            }
        }
    }

    pb.finish_with_message(format!(
        "Found {} dirs, {} files, {} symlinks",
        dirs.len(),
        files.len(),
        symlinks.len()
    ));

    // Optionally build dictionary (needs enough sample data)
    let total_content_size: usize = files.iter().map(|f| f.content.len()).sum();
    let dict = if opts.use_dict && files.len() > 1 && total_content_size > 4096 {
        let pb = ProgressBar::new_spinner();
        pb.set_message("Building dictionary...");
        let samples: Vec<Vec<u8>> = files.iter().map(|f| f.content.clone()).collect();
        let dict_size = 1_048_576.min(total_content_size / 2);
        match compress::build_dictionary(&samples, dict_size) {
            Ok(dict) => {
                pb.finish_with_message("Dictionary built");
                Some(dict)
            }
            Err(e) => {
                pb.finish_with_message(format!("Dictionary skipped: {e}"));
                None
            }
        }
    } else {
        None
    };

    // Compress files in parallel
    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=> "),
    );
    pb.set_message("Compressing files...");

    let compressed_files: Vec<CompressedFile> = files
        .par_iter()
        .map(|f| {
            let content_hash: [u8; 32] = *blake3::hash(&f.content).as_bytes();

            let blocks = if let Some(ref d) = dict {
                compress::compress_in_blocks(&f.content, opts.level, Some(d))
            } else {
                compress::compress_in_blocks(&f.content, opts.level, None)
            }
            .expect("compression failed");

            pb.inc(1);
            CompressedFile {
                rel_path: f.rel_path.clone(),
                blocks,
                content_hash,
                mode: f.mode,
                mtime_secs: f.mtime_secs,
                mtime_nsec: f.mtime_nsec,
            }
        })
        .collect();

    pb.finish_with_message("Compression complete");

    // Build string table and entry list
    let mut strings = StringTableBuilder::new();

    // Map path -> entry index for parent resolution
    let mut path_to_index: HashMap<PathBuf, u32> = HashMap::new();
    let mut entries: Vec<Entry> = Vec::new();

    // Add root entry
    let root_name = strings.add("");
    entries.push(Entry {
        kind: EntryKind::Dir,
        parent: u32::MAX,
        name: root_name,
        mode: 0o755,
        mtime_secs: 0,
        mtime_nsec: 0,
        content_hash: [0; 32],
        num_blocks: 0,
        blocks: Vec::new(),
        symlink_target: 0,
    });
    path_to_index.insert(PathBuf::new(), 0);

    // Sort dirs by depth so parents come first
    let mut sorted_dirs = dirs;
    sorted_dirs.sort_by_key(|d| d.rel_path.components().count());

    for d in &sorted_dirs {
        let name_str = d.rel_path.file_name().unwrap().to_str().unwrap();
        let name = strings.add(name_str);
        let parent_path = d.rel_path.parent().unwrap_or(Path::new(""));
        let parent = *path_to_index.get(parent_path).unwrap_or(&0);
        let idx = entries.len() as u32;
        entries.push(Entry {
            kind: EntryKind::Dir,
            parent,
            name,
            mode: d.mode,
            mtime_secs: d.mtime_secs,
            mtime_nsec: d.mtime_nsec,
            content_hash: [0; 32],
            num_blocks: 0,
            blocks: Vec::new(),
            symlink_target: 0,
        });
        path_to_index.insert(d.rel_path.clone(), idx);
    }

    // Compute payload layout - blocks are laid out sequentially
    let mut payload_offset: u64 = 0;
    let mut file_entry_indices: Vec<u32> = Vec::new();

    for cf in &compressed_files {
        let name_str = cf.rel_path.file_name().unwrap().to_str().unwrap();
        let name = strings.add(name_str);
        let parent_path = cf.rel_path.parent().unwrap_or(Path::new(""));
        let parent = *path_to_index.get(parent_path).unwrap_or(&0);
        let idx = entries.len() as u32;

        // Convert compressed blocks to onelf_format::Block with payload offsets
        let blocks: Vec<onelf_format::Block> = cf
            .blocks
            .iter()
            .map(|b| {
                let block = onelf_format::Block {
                    payload_offset,
                    compressed_size: b.data.len() as u64,
                    original_size: b.original_size,
                };
                payload_offset += b.data.len() as u64;
                block
            })
            .collect();

        entries.push(Entry {
            kind: EntryKind::File,
            parent,
            name,
            mode: cf.mode,
            mtime_secs: cf.mtime_secs,
            mtime_nsec: cf.mtime_nsec,
            content_hash: cf.content_hash,
            num_blocks: blocks.len() as u32,
            blocks,
            symlink_target: 0,
        });
        path_to_index.insert(cf.rel_path.clone(), idx);
        file_entry_indices.push(idx);
    }

    for sl in &symlinks {
        let name_str = sl.rel_path.file_name().unwrap().to_str().unwrap();
        let name = strings.add(name_str);
        let target_str = sl.target.to_str().unwrap();
        let target = strings.add(target_str);
        let parent_path = sl.rel_path.parent().unwrap_or(Path::new(""));
        let parent = *path_to_index.get(parent_path).unwrap_or(&0);
        entries.push(Entry {
            kind: EntryKind::Symlink,
            parent,
            name,
            mode: sl.mode,
            mtime_secs: sl.mtime_secs,
            mtime_nsec: sl.mtime_nsec,
            content_hash: [0; 32],
            num_blocks: 0,
            blocks: Vec::new(),
            symlink_target: target,
        });
    }

    // Build entrypoints
    let mut entrypoints: Vec<EntryPoint> = Vec::new();

    // Find the command file entry
    let command_path = PathBuf::from(&opts.command);
    let command_entry_idx = *path_to_index.get(&command_path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("command path '{}' not found in directory", opts.command),
        )
    })?;

    let default_name = opts
        .default_entrypoint
        .as_deref()
        .or_else(|| command_path.file_name().and_then(|n| n.to_str()))
        .unwrap_or("main");

    let ep_name = strings.add(default_name);
    let empty_args = strings.add("");

    let memfd_flag = match opts.memfd {
        Some(true) => EntryPointFlags::MEMFD_ELIGIBLE,
        _ => EntryPointFlags::empty(),
    };

    entrypoints.push(EntryPoint {
        name: ep_name,
        target_entry: command_entry_idx,
        args: empty_args,
        working_dir: opts.working_dir,
        flags: memfd_flag,
    });

    // Additional entrypoints
    for (name, path) in &opts.entrypoints {
        let ep_path = PathBuf::from(path);
        let ep_entry_idx = *path_to_index.get(&ep_path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("entrypoint path '{}' not found in directory", path),
            )
        })?;
        let ep_name = strings.add(name);
        entrypoints.push(EntryPoint {
            name: ep_name,
            target_entry: ep_entry_idx,
            args: empty_args,
            working_dir: opts.working_dir,
            flags: EntryPointFlags::empty(),
        });
    }

    // Add package name to string table
    let name_offset = strings.add(package_name) as u16;

    // Add lib dirs to string table
    let lib_dir_offsets: Vec<u32> = lib_dirs.iter().map(|d| strings.add(d)).collect();

    let string_table = strings.finish();

    // Build manifest and compute package_id
    let manifest = Manifest {
        header: ManifestHeader {
            version: 1,
            entry_count: entries.len() as u32,
            string_table_size: string_table.len() as u32,
            entrypoint_count: entrypoints.len() as u16,
            default_entrypoint: 0,
            lib_dir_count: lib_dir_offsets.len() as u16,
            name_offset,
            package_id: [0; 32], // placeholder, computed below
        },
        entrypoints,
        entries,
        lib_dir_offsets,
        string_table,
    };

    let mut manifest_bytes = manifest.serialize()?;

    // Compute package_id as BLAKE3 of manifest (with zeroed package_id field)
    let package_id: [u8; 32] = *blake3::hash(&manifest_bytes).as_bytes();
    // Patch the package_id in the serialized manifest (bytes 18..50 in header)
    manifest_bytes[18..50].copy_from_slice(&package_id);

    // Compress manifest
    let manifest_compressed = compress::compress_manifest(&manifest_bytes)?;

    // Compute manifest checksum (xxhash32 of uncompressed manifest)
    let manifest_checksum = xxhash_rust::xxh32::xxh32(&manifest_bytes, 0).to_le_bytes();

    // Compute total payload size
    let total_payload: u64 = compressed_files
        .iter()
        .map(|f| f.blocks.iter().map(|b| b.data.len() as u64).sum::<u64>())
        .sum();

    // Build flags
    let mut flags = Flags::empty();
    if dict.is_some() {
        flags |= Flags::HAS_DICT;
    }
    if opts.memfd == Some(true) {
        flags |= Flags::MEMFD_HINT;
    }

    // Write the output file
    let pb = ProgressBar::new_spinner();
    pb.set_message("Writing output...");

    let runtime_size = runtime_binary.len() as u64;
    let manifest_offset = runtime_size;
    let payload_start = manifest_offset + manifest_compressed.len() as u64;
    let dict_offset;
    let dict_size;

    if let Some(ref d) = dict {
        dict_offset = payload_start + total_payload;
        dict_size = d.len() as u32;
    } else {
        dict_offset = 0;
        dict_size = 0;
    }

    let footer = Footer {
        format_version: 1,
        flags,
        manifest_offset,
        manifest_compressed: manifest_compressed.len() as u64,
        manifest_original: manifest_bytes.len() as u64,
        payload_offset: payload_start,
        payload_size: total_payload,
        dict_offset,
        dict_size,
        manifest_checksum,
    };

    let out = File::create(&opts.output)?;
    let mut w = BufWriter::new(out);

    // [Runtime ELF]
    // Patch ELF header with ONELF signature in e_ident padding (bytes 9-14)
    let mut runtime_patched = runtime_binary.to_vec();
    if runtime_patched.len() >= 16 {
        // Bytes 9-14 are EI_PAD (padding), we can use them for signature
        // Signature: "ONELF\x00" (6 bytes)
        runtime_patched[9..15].copy_from_slice(b"ONELF\x00");
    }
    w.write_all(&runtime_patched)?;
    // [Manifest (compressed)]
    w.write_all(&manifest_compressed)?;
    // [Payload (concatenated compressed blocks)]
    for cf in &compressed_files {
        for block in &cf.blocks {
            w.write_all(&block.data)?;
        }
    }
    // [Dictionary (optional)]
    if let Some(ref d) = dict {
        w.write_all(d)?;
    }
    // [Footer]
    footer.write_to(&mut w)?;

    w.flush()?;
    drop(w);

    // Make output executable
    let perms = fs::Permissions::from_mode(0o755);
    fs::set_permissions(&opts.output, perms)?;

    let output_size = fs::metadata(&opts.output).map(|m| m.len()).unwrap_or(0);

    pb.finish_with_message(format!("Written to {}", opts.output.display()));

    // Summary
    let total_input = total_content_size as u64;
    let file_count = compressed_files.len();
    let dir_count = sorted_dirs.len();
    let symlink_count = symlinks.len();

    eprintln!();
    eprintln!(
        "  {}   {} files, {} dirs, {} symlinks",
        bold("Input:"),
        file_count,
        dir_count,
        symlink_count
    );
    eprintln!("  {} {}", bold("Content:"), format_size(total_input));
    eprintln!(
        "  {} {} (zstd level {})",
        bold("Payload:"),
        format_size(total_payload),
        opts.level
    );
    if let Some(ref d) = dict {
        eprintln!("  {}    {}", bold("Dict:"), format_size(d.len() as u64));
    }
    eprintln!("  {} {}", bold("Runtime:"), format_size(runtime_size));
    eprintln!(
        "  {}  {} (ratio: {:.2}x)",
        bold("Output:"),
        format_size(output_size),
        if output_size > 0 {
            total_input as f64 / output_size as f64
        } else {
            0.0
        }
    );

    Ok(())
}

fn bold(s: &str) -> String {
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        format!("\x1b[1m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn get_mtime(meta: &fs::Metadata) -> (u64, u32) {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0))
}

/// Parse PT_INTERP from ELF data, returning the interpreter path.
fn elf_interp(data: &[u8]) -> Option<String> {
    let info = elf_interp_info(data)?;
    Some(info.2)
}

/// Find PT_INTERP info: (offset, max_len, original_path).
fn elf_interp_info(data: &[u8]) -> Option<(usize, usize, String)> {
    if data.len() < 64 || data[0..4] != *b"\x7fELF" {
        return None;
    }

    let class = data[4];
    let (e_phoff, e_phentsize, e_phnum) = match class {
        2 => {
            let e_phoff = u64::from_le_bytes(data[32..40].try_into().ok()?) as usize;
            let e_phentsize = u16::from_le_bytes(data[54..56].try_into().ok()?) as usize;
            let e_phnum = u16::from_le_bytes(data[56..58].try_into().ok()?) as usize;
            (e_phoff, e_phentsize, e_phnum)
        }
        1 => {
            let e_phoff = u32::from_le_bytes(data[28..32].try_into().ok()?) as usize;
            let e_phentsize = u16::from_le_bytes(data[42..44].try_into().ok()?) as usize;
            let e_phnum = u16::from_le_bytes(data[44..46].try_into().ok()?) as usize;
            (e_phoff, e_phentsize, e_phnum)
        }
        _ => return None,
    };

    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        if off + e_phentsize > data.len() {
            break;
        }

        let p_type = u32::from_le_bytes(data[off..off + 4].try_into().ok()?);
        if p_type != 3 {
            continue;
        }

        let (p_offset, p_filesz) = match class {
            2 => {
                let o = u64::from_le_bytes(data[off + 8..off + 16].try_into().ok()?) as usize;
                let s = u64::from_le_bytes(data[off + 32..off + 40].try_into().ok()?) as usize;
                (o, s)
            }
            1 => {
                let o = u32::from_le_bytes(data[off + 4..off + 8].try_into().ok()?) as usize;
                let s = u32::from_le_bytes(data[off + 16..off + 20].try_into().ok()?) as usize;
                (o, s)
            }
            _ => return None,
        };

        if p_offset + p_filesz > data.len() {
            return None;
        }

        let interp = &data[p_offset..p_offset + p_filesz];
        // Truncate at first NUL — patched binaries pad the shorter new path with NULs
        let interp = match interp.iter().position(|&b| b == 0) {
            Some(pos) => &interp[..pos],
            None => interp,
        };
        let path = std::str::from_utf8(interp).ok()?.to_string();
        return Some((p_offset, p_filesz, path));
    }

    None
}

/// Patch PT_INTERP in ELF data to point to a new path. Returns true if patched.
fn patch_elf_interp(data: &mut [u8], new_interp: &str) -> bool {
    let (offset, max_len, original) = match elf_interp_info(data) {
        Some(info) => info,
        None => return false,
    };

    if original == new_interp {
        return false;
    }

    let needed = new_interp.len() + 1;
    if needed > max_len {
        return false;
    }

    let dest = &mut data[offset..offset + max_len];
    dest[..new_interp.len()].copy_from_slice(new_interp.as_bytes());
    for b in &mut dest[new_interp.len()..] {
        *b = 0;
    }

    true
}
