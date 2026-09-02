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
use crate::payload::ONELF_ENV_SONAME;
use onelf_format::HostLibsPolicy;
use std::borrow::Cow;
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

/// What the launch resolver may take from the host.
///
/// The runtime compares each library present in both the bundle and on
/// the host and loads whichever carries the newer symbol versions. The
/// policy bounds which sonames it compares at all: only the driver
/// stack's host closure, every bundled soname, or nothing. Packages that
/// need nothing from the host are better off with nothing, so that a
/// missing soname fails by name instead of being quietly satisfied by a
/// host copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostLibs {
    /// Decide from the bundle's contents: `auto` when a driver stack is
    /// referenced, `never` otherwise.
    #[default]
    Auto,
    /// Compare every bundled soname against the host.
    Always,
    /// Take nothing from the host.
    Never,
}

pub struct PackOptions {
    pub directory: PathBuf,
    pub output: PathBuf,
    pub command: String,
    pub name: Option<String>,
    /// (name, path, args) tuples for additional entrypoints.
    pub entrypoints: Vec<(String, String, Vec<String>)>,
    pub default_entrypoint: Option<String>,
    pub lib_dirs: Vec<String>,
    pub level: i32,
    /// Bytes per payload block. See [`compress::DEFAULT_BLOCK_SIZE`].
    pub block_size: u64,
    pub use_dict: bool,
    /// Store the payload raw (no zstd). Mutually exclusive with a
    /// dictionary; trades file size for zero decompression at runtime.
    pub no_compress: bool,
    pub memfd: Option<bool>,
    pub working_dir: WorkingDir,
    /// What the launch resolver may take from the host. See [`HostLibs`].
    pub host_libs: HostLibs,
    /// Ask the runtime to fall back to the persistent cache when no
    /// other execution mode works. Without it the runtime stops at the
    /// runtime directory and reports failure, leaving nothing on disk.
    pub cache: bool,
    pub update_url: Option<String>,
    /// Embed the update-capable runtime when `update_url` is set. False
    /// records the update metadata but links the slim runtime, for
    /// packages updated by a package manager rather than by themselves.
    pub embed_updater: bool,
    /// Raw Ed25519 public key bytes embedded as `.onelf/update-key`; the
    /// runtime requires it (and a valid signature) before self-updating.
    pub update_key: Option<Vec<u8>>,
    pub exclude: Vec<String>,
    /// Optional TOML-formatted metadata written to `.onelf/package-info.toml`.
    pub package_info: Option<String>,
    /// Pin every entry's mtime to this Unix timestamp (nsec 0) for fully
    /// reproducible output independent of filesystem timestamps.
    pub mtime: Option<u64>,
    /// Custom environment variables to set before exec (KEY=VALUE pairs).
    pub env: Vec<(String, String)>,
    /// Libraries to dlopen on every exec, written to `.onelf/preload` and
    /// applied by the bundled onelf-env constructor. `${ONELF_DIR}`
    /// expands to the package root at runtime.
    pub preload: Vec<String>,
    /// Whether the app runs setuid binaries, written as `.onelf/needs-setuid`
    /// and read by the runtime before it picks how to unpack itself. Keeps the
    /// app out of a user namespace, where a setuid bit does nothing.
    pub needs_setuid: bool,
}

/// Where a file's bytes come from when they are finally needed.
///
/// Holding every file's content in memory made peak usage a multiple of the
/// input tree, which capped the packer well below the sizes it is meant for.
/// Real files are read during the content pass and dropped again; only the
/// synthetic `.onelf/*` metadata, which is small and has no file behind it,
/// is carried in memory.
enum FileSource {
    Disk(PathBuf),
    Memory(Vec<u8>),
}

impl FileSource {
    fn read(&self) -> io::Result<Cow<'_, [u8]>> {
        match self {
            FileSource::Disk(path) => Ok(Cow::Owned(fs::read(path)?)),
            FileSource::Memory(bytes) => Ok(Cow::Borrowed(bytes)),
        }
    }
}

struct CollectedFile {
    rel_path: PathBuf,
    source: FileSource,
    /// Size in bytes, recorded during the metadata walk so the content pass
    /// can bound how much it loads without stat-ing again.
    size: u64,
    mode: u32,
    mtime_secs: u64,
    mtime_nsec: u32,
}

struct CompressedFile {
    rel_path: PathBuf,
    blocks: Vec<onelf_format::Block>,
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

/// Package-relative path the default entrypoint launches.
///
/// A recipe may point the default at a declared `[[entrypoint]]` rather than
/// at the package command, and the two can name different binaries.
fn default_entrypoint_path(opts: &PackOptions) -> PathBuf {
    opts.default_entrypoint
        .as_deref()
        .and_then(|name| opts.entrypoints.iter().find(|(n, _, _)| n == name))
        .map(|(_, path, _)| PathBuf::from(path))
        .unwrap_or_else(|| PathBuf::from(&opts.command))
}

/// Bytes of file content held in flight during compression.
///
/// This is what bounds the packer independently of tree size. Counting files
/// instead of bytes does not: a chunk of 64 files is most of a tree that has
/// 82 of them, so the whole thing ends up resident anyway.
///
/// Peak usage is roughly this budget plus the compressed output of the same
/// chunk. Override with `ONELF_PACK_CHUNK_BYTES`.
fn compression_budget() -> u64 {
    const DEFAULT: u64 = 64 * 1024 * 1024;
    std::env::var("ONELF_PACK_CHUNK_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT)
}

/// Split `files` into runs whose total size stays within `budget`.
///
/// A file larger than the budget forms a run of its own rather than being
/// skipped, since it still has to be compressed. Order is preserved, so
/// payload offsets remain a function of the sorted walk.
fn size_bounded_chunks(files: &[CollectedFile], budget: u64) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = 0usize;
    let mut acc = 0u64;
    for (i, f) in files.iter().enumerate() {
        if i > start && acc + f.size > budget {
            runs.push((start, i));
            start = i;
            acc = 0;
        }
        acc += f.size;
    }
    if start < files.len() {
        runs.push((start, files.len()));
    }
    runs
}

/// Total bytes of sample fed to dictionary training.
///
/// zstd's guidance is roughly 100x the dictionary size, and the dictionary is
/// capped at 1 MiB, so this is already generous. Sampling matters because the
/// previous implementation copied every file twice: once to build the sample
/// list and again to flatten it.
const DICT_SAMPLE_BUDGET: usize = 100 * 1024 * 1024;

/// Per-file cap on the sample, so one huge file cannot crowd out the variety
/// that makes a dictionary useful.
const DICT_SAMPLE_PER_FILE: usize = 1024 * 1024;

/// Train a zstd dictionary from a bounded, deterministic sample.
///
/// Files are taken in the walk's sorted order until the budget is spent, so
/// the same tree always trains the same dictionary and output stays
/// reproducible.
fn build_dictionary_sampled(files: &[CollectedFile], dict_size: usize) -> io::Result<Vec<u8>> {
    let mut samples: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    for f in files {
        if total >= DICT_SAMPLE_BUDGET {
            break;
        }
        let content = f.source.read()?;
        if content.is_empty() {
            continue;
        }
        let take = content.len().min(DICT_SAMPLE_PER_FILE);
        samples.push(content[..take].to_vec());
        total += take;
    }
    if samples.len() < 2 {
        return Err(io::Error::other("not enough sample data"));
    }
    compress::build_dictionary(&samples, dict_size)
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

/// The final path component of `p` as UTF-8, or a descriptive error on a
/// non-UTF-8 name (the string table stores UTF-8, so packing must reject
/// such names rather than panic).
fn utf8_file_name(p: &Path) -> io::Result<&str> {
    p.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("non-UTF-8 file name: {}", p.display()),
        )
    })
}

/// `p` as a UTF-8 string, or a descriptive error on non-UTF-8 input.
fn utf8_str(p: &Path) -> io::Result<&str> {
    p.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("non-UTF-8 path: {}", p.display()),
        )
    })
}

/// Inject a synthetic `.onelf/*` metadata file, creating the `.onelf`
/// directory entry once if it is not already present. `rel_path` is the
/// full package-relative path (e.g. `.onelf/env`); both the directory and
/// the file are stamped with `mtime` for reproducible output.
fn inject_onelf_file(
    dirs: &mut Vec<CollectedDir>,
    files: &mut Vec<CollectedFile>,
    rel_path: &str,
    content: Vec<u8>,
    mtime: u64,
) {
    if !dirs.iter().any(|d| d.rel_path == Path::new(".onelf")) {
        dirs.push(CollectedDir {
            rel_path: PathBuf::from(".onelf"),
            mode: 0o755,
            mtime_secs: mtime,
            mtime_nsec: 0,
        });
    }
    let size = content.len() as u64;
    files.push(CollectedFile {
        rel_path: PathBuf::from(rel_path),
        source: FileSource::Memory(content),
        size,
        mode: 0o644,
        mtime_secs: mtime,
        mtime_nsec: 0,
    });
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
        let entry = entry.map_err(io::Error::other)?;
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
        let (mtime_secs, mtime_nsec) = get_mtime(&symlink_meta, opts.mtime);
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
            files.push(CollectedFile {
                rel_path,
                source: FileSource::Disk(abs_path.clone()),
                size: symlink_meta.len(),
                mode,
                mtime_secs,
                mtime_nsec,
            });
        }
    }

    // Injected .onelf metadata is synthetic (no filesystem mtime), so
    // honor an explicit `--mtime` pin for it too; otherwise keep 0 so it
    // stays stable across runs.
    let inject_mtime = opts.mtime.unwrap_or(0);

    // `.onelf/` is reserved for injected metadata, except for the
    // documented user-provided asset dirs `.onelf/icons/` and
    // `.onelf/desktop/` (resolved by `integrate`/`icon`/`desktop`) and the
    // provenance record `bundle-libs` writes from a sysroot. The bare
    // `.onelf` directory is allowed as their parent. Any other collision
    // is rejected so injected metadata is never shadowed by, or silently
    // duplicated alongside, source content.
    let reserved = Path::new(".onelf");
    let collision = files
        .iter()
        .map(|f| &f.rel_path)
        .chain(dirs.iter().map(|d| &d.rel_path))
        .chain(symlinks.iter().map(|s| &s.rel_path))
        .find(|p| {
            p.starts_with(reserved)
                && p.as_path() != reserved
                && !p.starts_with(".onelf/icons")
                && !p.starts_with(".onelf/desktop")
                && p.as_path() != Path::new(crate::bundle::sysroot::PROVENANCE_FILE)
        });
    if let Some(p) = collision {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "source contains reserved path {}; the .onelf/ namespace is injected by onelf",
                p.display()
            ),
        ));
    }

    // Inject .onelf/update-url if requested
    if let Some(ref url) = opts.update_url {
        inject_onelf_file(
            &mut dirs,
            &mut files,
            ".onelf/update-url",
            url.as_bytes().to_vec(),
            inject_mtime,
        );
    }

    // Inject .onelf/update-key (raw Ed25519 public key) if provided.
    if let Some(ref key) = opts.update_key {
        if key.len() != 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "update key must be a 32-byte Ed25519 public key, got {} bytes",
                    key.len()
                ),
            ));
        }
        inject_onelf_file(
            &mut dirs,
            &mut files,
            ".onelf/update-key",
            key.clone(),
            inject_mtime,
        );
    }

    // Inject .onelf/package-info.toml if requested
    if let Some(ref info) = opts.package_info {
        inject_onelf_file(
            &mut dirs,
            &mut files,
            ".onelf/package-info.toml",
            info.as_bytes().to_vec(),
            inject_mtime,
        );
    }

    let package_name = opts
        .name
        .as_deref()
        .unwrap_or_else(|| opts.command.split('/').next_back().unwrap_or("app"));

    // Detect bundled ELF interpreter for cross-libc portability.
    // When a matching interpreter is bundled (e.g. via bundle-libs), record its
    // relative path in .onelf/interp so the runtime can use userland-exec with
    // the bundled interpreter instead of the system one.
    {
        // Read the interpreter off the entrypoint itself. Taking whichever
        // ELF the walk reaches first can pick up a helper binary built
        // against a different libc and record its loader instead.
        //
        // A bundled entrypoint often has no PT_INTERP left, because the
        // bootstrap injection repurposed it, and then nothing is recorded.
        // That is correct: the runtime only consults this file for targets
        // that still carry an interpreter.
        let entry_target = default_entrypoint_path(opts);
        let original_interp = files
            .iter()
            .find(|f| f.rel_path == entry_target)
            .and_then(|f| f.source.read().ok())
            .and_then(|c| elf_interp(&c));
        let bundled_relpath = original_interp.as_ref().and_then(|interp| {
            let interp_name = Path::new(interp).file_name()?.to_str()?;
            let match_name = |p: &Path| p.file_name().and_then(|n| n.to_str()) == Some(interp_name);
            files
                .iter()
                .find(|f| match_name(&f.rel_path))
                .map(|f| f.rel_path.to_string_lossy().into_owned())
                .or_else(|| {
                    symlinks
                        .iter()
                        .find(|s| match_name(&s.rel_path))
                        .map(|s| s.rel_path.to_string_lossy().into_owned())
                })
        });

        if let Some(bundled_rel) = bundled_relpath {
            inject_onelf_file(
                &mut dirs,
                &mut files,
                ".onelf/interp",
                bundled_rel.into_bytes(),
                inject_mtime,
            );
        }
    }

    // Write custom env vars as .onelf/env (KEY=VALUE per line).
    //
    // By default the package's own bin/ is prepended to PATH, applied
    // re-exec-safely (onelf-env constructor) and on first launch
    // (onelf-rt). `${PATH}` is expanded against the live environment at
    // runtime, so this prepends rather than replaces. A recipe that
    // sets `[env] PATH` itself takes full control and the default is
    // skipped (use `PATH = "$${PATH}"` to opt out of the bin prefix
    // entirely).
    {
        let user_sets_path = opts.env.iter().any(|(k, _)| k == "PATH");
        let mut env_lines: Vec<String> = Vec::new();
        if !user_sets_path {
            // `${PATH:-/usr/bin:/bin}`: keep the inherited PATH, but
            // once we set PATH at all glibc no longer applies its
            // _CS_PATH (/bin:/usr/bin) fallback, so substitute it
            // ourselves when the inherited PATH is empty (e.g. after a
            // sandbox clearenv()) instead of leaving a dangling ":".
            env_lines.push("PATH=${ONELF_DIR}/bin:${PATH:-/usr/bin:/bin}".to_string());
        }
        for (k, v) in &opts.env {
            env_lines.push(format!("{k}={v}"));
        }
        inject_onelf_file(
            &mut dirs,
            &mut files,
            ".onelf/env",
            env_lines.join("\n").into_bytes(),
            inject_mtime,
        );
    }

    // Write preload library list as .onelf/preload (one path per line),
    // applied by the bundled onelf-env constructor.
    if !opts.preload.is_empty() {
        inject_onelf_file(
            &mut dirs,
            &mut files,
            ".onelf/preload",
            opts.preload.join("\n").into_bytes(),
            inject_mtime,
        );
    }

    // The file being there is the whole message, so it carries nothing. The
    // runtime looks for it before choosing how to unpack, which is earlier
    // than anything can be read out of the package proper.
    if opts.needs_setuid {
        inject_onelf_file(
            &mut dirs,
            &mut files,
            ".onelf/needs-setuid",
            Vec::new(),
            inject_mtime,
        );
    }

    pb.finish_with_message(format!(
        "Found {} dirs, {} files, {} symlinks",
        dirs.len(),
        files.len(),
        symlinks.len()
    ));

    let total_content_size: u64 = files
        .iter()
        .map(|f| match &f.source {
            FileSource::Disk(p) => fs::metadata(p).map(|m| m.len()).unwrap_or(0),
            FileSource::Memory(b) => b.len() as u64,
        })
        .sum();

    let dict = if opts.no_compress {
        // Store mode writes raw bytes; a dictionary would be unused.
        if opts.use_dict {
            eprintln!("note: store mode overrides dictionary; payload stored raw");
        }
        None
    } else if opts.use_dict && files.len() > 1 && total_content_size > 4096 {
        let pb = ProgressBar::new_spinner();
        pb.set_message("Building dictionary...");
        let dict_size = 1_048_576.min(total_content_size as usize / 2);
        match build_dictionary_sampled(&files, dict_size) {
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

    // Compress in bounded chunks, streaming each file's blocks out to a temp
    // payload as they are produced. Holding the whole tree, or the whole
    // compressed payload, made peak memory a multiple of the input size.
    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=> "),
    );
    pb.set_message(if opts.no_compress {
        "Storing files..."
    } else {
        "Compressing files..."
    });

    let payload_tmp_path = opts.output.with_extension("onelf-payload.tmp");
    let mut payload_tmp = BufWriter::new(File::create(&payload_tmp_path)?);
    let mut payload_offset: u64 = 0;
    let mut compressed_files: Vec<CompressedFile> = Vec::with_capacity(files.len());
    // Identical content is written once and shared, so a tree with repeated
    // files pays for them once rather than per path.
    let mut seen: HashMap<[u8; 32], Vec<onelf_format::Block>> = HashMap::new();
    let mut deduped_bytes: u64 = 0;

    let result = (|| -> io::Result<()> {
        for (from, to) in size_bounded_chunks(&files, compression_budget()) {
            let chunk = &files[from..to];
            let done: Vec<(usize, [u8; 32], Vec<compress::CompressedBlock>)> = chunk
                .par_iter()
                .enumerate()
                .map(|(i, f)| -> io::Result<_> {
                    let content = f.source.read()?;
                    let hash: [u8; 32] = *blake3::hash(&content).as_bytes();
                    let blocks = if opts.no_compress {
                        compress::store_in_blocks(&content, opts.block_size)
                    } else {
                        compress::compress_in_blocks(
                            &content,
                            opts.level,
                            dict.as_deref(),
                            opts.block_size,
                        )
                        .map_err(|e| {
                            io::Error::other(format!("compress {}: {e}", f.rel_path.display()))
                        })?
                    };
                    pb.inc(1);
                    Ok((i, hash, blocks))
                })
                .collect::<io::Result<Vec<_>>>()?;

            // Written back in chunk order, so payload offsets stay a
            // function of the sorted input and output remains reproducible.
            let mut done = done;
            done.sort_by_key(|(i, _, _)| *i);
            for (i, content_hash, raw) in done {
                let f = &chunk[i];
                let blocks = match seen.get(&content_hash) {
                    Some(shared) => {
                        deduped_bytes += raw.iter().map(|b| b.data.len() as u64).sum::<u64>();
                        shared.clone()
                    }
                    None => {
                        let mut blocks = Vec::with_capacity(raw.len());
                        for b in &raw {
                            payload_tmp.write_all(&b.data)?;
                            blocks.push(onelf_format::Block {
                                payload_offset,
                                compressed_size: b.data.len() as u64,
                                original_size: b.original_size,
                                content_hash: b.content_hash,
                            });
                            payload_offset += b.data.len() as u64;
                        }
                        seen.insert(content_hash, blocks.clone());
                        blocks
                    }
                };
                compressed_files.push(CompressedFile {
                    rel_path: f.rel_path.clone(),
                    blocks,
                    content_hash,
                    mode: f.mode,
                    mtime_secs: f.mtime_secs,
                    mtime_nsec: f.mtime_nsec,
                });
            }
        }
        payload_tmp.flush()
    })();
    drop(payload_tmp);
    if let Err(e) = result {
        let _ = fs::remove_file(&payload_tmp_path);
        return Err(e);
    }

    pb.finish_with_message(if opts.no_compress {
        "Files stored (uncompressed)"
    } else {
        "Compression complete"
    });

    // Build string table and entry list
    let mut strings = StringTableBuilder::new();

    // Map path -> entry index for parent resolution
    let mut path_to_index: HashMap<PathBuf, u32> = HashMap::new();
    let mut entries: Vec<Entry> = Vec::new();

    // Add the root entry name ("") at offset 0, then the package name next
    // so its offset stays small and fits the `u16` header field however
    // large the rest of the string table grows. Offset 0 is
    // also the "unset" sentinel, so a non-empty name never lands there.
    let root_name = strings.add("");
    let name_offset = u16::try_from(strings.add(package_name)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "package name offset exceeds u16 (string table too large)",
        )
    })?;
    entries.push(Entry {
        kind: EntryKind::Dir,
        parent: u32::MAX,
        name: root_name,
        mode: 0o755,
        mtime_secs: 0,
        mtime_nsec: 0,
        content_hash: [0; 32],
        blocks: Vec::new(),
        symlink_target: 0,
    });
    path_to_index.insert(PathBuf::new(), 0);

    // Sort dirs by depth so parents come first
    let mut sorted_dirs = dirs;
    sorted_dirs.sort_by_key(|d| d.rel_path.components().count());

    for d in &sorted_dirs {
        let name_str = utf8_file_name(&d.rel_path)?;
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
            blocks: Vec::new(),
            symlink_target: 0,
        });
        path_to_index.insert(d.rel_path.clone(), idx);
    }

    // Block offsets were assigned as the payload was streamed out, so the
    // entries only have to carry them.
    for cf in &compressed_files {
        let name_str = utf8_file_name(&cf.rel_path)?;
        let name = strings.add(name_str);
        let parent_path = cf.rel_path.parent().unwrap_or(Path::new(""));
        let parent = *path_to_index.get(parent_path).unwrap_or(&0);
        let idx = entries.len() as u32;
        let blocks = cf.blocks.clone();

        entries.push(Entry {
            kind: EntryKind::File,
            parent,
            name,
            mode: cf.mode,
            mtime_secs: cf.mtime_secs,
            mtime_nsec: cf.mtime_nsec,
            content_hash: cf.content_hash,
            blocks,
            symlink_target: 0,
        });
        path_to_index.insert(cf.rel_path.clone(), idx);
    }

    for sl in &symlinks {
        let name_str = utf8_file_name(&sl.rel_path)?;
        let name = strings.add(name_str);
        let target_str = utf8_str(&sl.target)?;
        let target = strings.add(target_str);
        let parent_path = sl.rel_path.parent().unwrap_or(Path::new(""));
        let parent = *path_to_index.get(parent_path).unwrap_or(&0);
        let idx = entries.len() as u32;
        entries.push(Entry {
            kind: EntryKind::Symlink,
            parent,
            name,
            mode: sl.mode,
            mtime_secs: sl.mtime_secs,
            mtime_nsec: sl.mtime_nsec,
            content_hash: [0; 32],
            blocks: Vec::new(),
            symlink_target: target,
        });
        // Registered like files and directories so an entrypoint may target
        // a symlink, which is how an AppDir usually exposes its launcher.
        path_to_index.insert(sl.rel_path.clone(), idx);
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

    // Two entrypoints answering to one name would leave which of them runs
    // up to resolution order, so the ambiguity is refused at pack time.
    if let Some(dup) = opts
        .entrypoints
        .iter()
        .enumerate()
        .find(|(i, (n, _, _))| opts.entrypoints[..*i].iter().any(|(m, _, _)| m == n))
        .map(|(_, (n, _, _))| n)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("duplicate entrypoint name '{dup}'"),
        ));
    }

    let default_name = opts
        .default_entrypoint
        .as_deref()
        .or_else(|| command_path.file_name().and_then(|n| n.to_str()))
        .unwrap_or("main");

    // When `default_entrypoint` names a declared `[[entrypoint]]`, the
    // default launch must use that entrypoint's path and args, not the
    // package command's, and the tuple must not also be appended below as a
    // second entrypoint with the same name.
    let default_decl = opts
        .default_entrypoint
        .as_deref()
        .and_then(|dn| opts.entrypoints.iter().find(|(n, _, _)| n == dn));

    // A default naming no declared entrypoint used to fall back to the
    // package command under the typed name, silently producing something the
    // recipe never described.
    if let Some(name) = opts.default_entrypoint.as_deref()
        && default_decl.is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("default entrypoint '{name}' matches no declared entrypoint"),
        ));
    }

    let ep_name = strings.add(default_name);
    let empty_args = strings.add("");

    let (ep0_target_idx, ep0_target_path) = match default_decl {
        Some((_, path, _)) => {
            let p = PathBuf::from(path);
            let idx = *path_to_index.get(&p).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("entrypoint path '{}' not found in directory", path),
                )
            })?;
            (idx, p)
        }
        None => (command_entry_idx, command_path.clone()),
    };
    let ep0_args = match default_decl {
        Some((_, _, args)) if !args.is_empty() => strings.add(&args.join("\x1f")),
        _ => empty_args,
    };

    // Memfd eligibility is evaluated against the entrypoint's real target.
    // Explicit --memfd=true/false overrides auto-detect. Auto: eligible if
    // the target has no DT_NEEDED dependencies (static musl/glibc or a
    // non-ELF script).
    let memfd_flag = match opts.memfd {
        Some(false) => EntryPointFlags::empty(),
        requested => {
            let target_content = files
                .iter()
                .find(|f| f.rel_path == ep0_target_path)
                .and_then(|f| f.source.read().ok());
            // `--memfd` overrides the auto-detect above, but it cannot override
            // physics: `bundle-libs` links the entrypoint against a library it
            // puts inside the package, and a memfd exec never puts anything on
            // disk for the loader to find. Such a package fails for every user,
            // at every launch, with an error naming a library rather than the
            // mode that made it unreachable. Refuse to build it.
            if requested == Some(true)
                && target_content.as_deref().is_some_and(elf_needs_bundled_lib)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "--memfd cannot be used with '{}': it is linked \
                         against {ONELF_ENV_SONAME}, which exists only inside the \
                         package, and memfd mode runs the entrypoint without \
                         extracting anything. Drop --memfd, or pack a target that \
                         bundle-libs has not processed.",
                        ep0_target_path.display()
                    ),
                ));
            }
            if requested == Some(true) || target_content.as_deref().is_some_and(elf_has_no_deps) {
                EntryPointFlags::MEMFD_ELIGIBLE
            } else {
                EntryPointFlags::empty()
            }
        }
    };

    entrypoints.push(EntryPoint {
        name: ep_name,
        target_entry: ep0_target_idx,
        args: ep0_args,
        working_dir: opts.working_dir,
        flags: memfd_flag,
    });

    // Additional entrypoints (skip the tuple already emitted as the default
    // above so its name is not duplicated).
    for (name, path, args) in &opts.entrypoints {
        if opts.default_entrypoint.as_deref() == Some(name.as_str()) {
            continue;
        }
        let ep_path = PathBuf::from(path);
        let ep_entry_idx = *path_to_index.get(&ep_path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("entrypoint path '{}' not found in directory", path),
            )
        })?;
        let ep_name = strings.add(name);
        let ep_args = if args.is_empty() {
            empty_args
        } else {
            strings.add(&args.join("\x1f"))
        };
        entrypoints.push(EntryPoint {
            name: ep_name,
            target_entry: ep_entry_idx,
            args: ep_args,
            working_dir: opts.working_dir,
            flags: EntryPointFlags::empty(),
        });
    }

    // Add lib dirs to string table
    let lib_dir_offsets: Vec<u32> = lib_dirs.iter().map(|d| strings.add(d)).collect();

    let string_table = strings.finish();

    // Build manifest and compute package_id
    let manifest = Manifest {
        header: ManifestHeader {
            version: onelf_format::manifest::MANIFEST_VERSION,
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
    // What was actually written, which after dedup is less than the sum of
    // every entry's blocks.
    let total_payload: u64 = payload_offset;

    // Build flags
    let mut flags = Flags::empty();
    if dict.is_some() {
        flags |= Flags::HAS_DICT;
    }
    if opts.memfd == Some(true) {
        flags |= Flags::MEMFD_HINT;
    }
    if opts.no_compress {
        flags |= Flags::STORED;
    }
    if opts.update_url.is_some() && !opts.embed_updater {
        flags |= Flags::EXTERNAL_UPDATER;
    }
    let policy = match opts.host_libs {
        HostLibs::Always => HostLibsPolicy::Always,
        HostLibs::Never => HostLibsPolicy::Never,
        HostLibs::Auto if bundle_needs_host_libs(&opts.directory) => HostLibsPolicy::Auto,
        HostLibs::Auto => HostLibsPolicy::Never,
    };
    flags |= policy.to_flags();
    if opts.cache {
        flags |= Flags::CACHE_REQUESTED;
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
    // [Payload], streamed back from the temp file rather than held in memory
    let mut payload_src = File::open(&payload_tmp_path)?;
    io::copy(&mut payload_src, &mut w)?;
    drop(payload_src);
    // [Dictionary (optional)]
    if let Some(ref d) = dict {
        w.write_all(d)?;
    }
    // [Footer]
    footer.write_to(&mut w)?;

    w.flush()?;
    drop(w);
    let _ = fs::remove_file(&payload_tmp_path);

    // Make output executable
    let perms = fs::Permissions::from_mode(0o755);
    fs::set_permissions(&opts.output, perms)?;

    let output_size = fs::metadata(&opts.output).map(|m| m.len()).unwrap_or(0);

    pb.finish_with_message(format!("Written to {}", opts.output.display()));

    // Summary
    let total_input = total_content_size;
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
    if deduped_bytes > 0 {
        eprintln!(
            "  {}   {} saved by sharing identical content",
            bold("Dedup:"),
            format_size(deduped_bytes)
        );
    }
    if opts.no_compress {
        eprintln!(
            "  {} {} (stored, uncompressed)",
            bold("Payload:"),
            format_size(total_payload)
        );
    } else {
        eprintln!(
            "  {} {} (zstd level {})",
            bold("Payload:"),
            format_size(total_payload),
            opts.level
        );
    }
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

fn get_mtime(meta: &fs::Metadata, pin: Option<u64>) -> (u64, u32) {
    // An explicit `--mtime` / `[package] mtime` pin overrides everything:
    // every entry gets the same timestamp, so output no longer depends on
    // filesystem mtimes at all.
    if let Some(ts) = pin {
        return (ts, 0);
    }

    let raw = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Reproducible-builds convention: if SOURCE_DATE_EPOCH is set, clamp
    // mtimes to it. Older files keep their original mtime; newer ones are
    // pinned to the epoch.
    if let Some(epoch) = source_date_epoch() {
        return (raw.min(epoch), 0);
    }

    // Nanosecond precision is always dropped so two checkouts of the same
    // content differ only by whole-second mtimes (further pinnable above).
    (raw, 0)
}

fn source_date_epoch() -> Option<u64> {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
}

/// True if the data is an ELF binary with zero DT_NEEDED dependencies.
/// Such entrypoints are self-contained and can run from a memfd.
///
/// Shell scripts and other non-ELF targets are rejected: they often rely
/// on dirname($0) or sibling files to locate resources, and both break
/// when the process is execed from /proc/self/fd/N.
fn elf_has_no_deps(data: &[u8]) -> bool {
    if data.len() < 4 || data[0..4] != *b"\x7fELF" {
        return false;
    }
    match goblin::elf::Elf::parse(data) {
        Ok(elf) => elf.libraries.is_empty(),
        Err(_) => false,
    }
}

/// Whether `data` is linked against a library only the package provides.
///
/// `bundle-libs` adds `libonelf-env.so` as a `DT_NEEDED` so the recipe's
/// environment is reapplied on every exec. It ships inside the package, which
/// makes the binary unrunnable in any mode that does not put the package on a
/// filesystem first.
fn elf_needs_bundled_lib(data: &[u8]) -> bool {
    if data.len() < 4 || data[0..4] != *b"\x7fELF" {
        return false;
    }
    match goblin::elf::Elf::parse(data) {
        Ok(elf) => elf.libraries.contains(&ONELF_ENV_SONAME),
        Err(_) => false,
    }
}

/// Interpreter path recorded in `data`'s `PT_INTERP`, if it has one.
///
/// Shares the runtime's bounds-checked program-header walk rather than
/// keeping a second, unchecked copy of the same parse.
fn elf_interp(data: &[u8]) -> Option<String> {
    let (offset, size) = onelf_format::elf::pt_interp_slot(data)?;
    let end = offset.checked_add(size)?;
    let raw = data.get(offset..end)?;
    let text = match raw.iter().position(|&b| b == 0) {
        Some(nul) => &raw[..nul],
        None => raw,
    };
    std::str::from_utf8(text).ok().map(String::from)
}

/// Whether a tree needs libraries the host must supply.
///
/// Deliberately generous. Guessing "no" when the answer is "yes" breaks an
/// app that works today, while guessing "yes" only leaves the current
/// behaviour in place, so anything that looks like a host-provided family
/// counts.
///
/// Driver stacks are loaded by `dlopen` at runtime and so never appear in
/// `DT_NEEDED`. Their sonames are looked for as strings anywhere in the
/// bundle's ELF files instead.
fn bundle_needs_host_libs(directory: &Path) -> bool {
    use onelf_format::drivers::DRIVER_FAMILIES;

    for entry in jwalk::WalkDir::new(directory).sort(true) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if DRIVER_FAMILIES.iter().any(|f| name.contains(f)) {
            return true;
        }
        // Magic first. A tree like Blender's is mostly Python and data
        // files, and reading those in full to discover they are not ELF
        // costs more than the whole rest of this scan.
        let Ok(mut file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut magic = [0u8; 4];
        if std::io::Read::read_exact(&mut file, &mut magic).is_err() || magic != *b"\x7fELF" {
            continue;
        }
        let mut data = magic.to_vec();
        if std::io::Read::read_to_end(&mut file, &mut data).is_err() {
            continue;
        }
        // NSS modules are dlopened by glibc for getpwnam, DNS and friends.
        // glibc 2.34 folded files and dns into libc.so.6 itself, so a
        // bundle shipping that or newer needs nothing from the host for
        // them. An older bundled glibc still does, and the version marker
        // is the reliable way to tell the two apart: verified that a 2.43
        // libc carries `GLIBC_2.34` and a 2.31 libc does not.
        if name == "libc.so.6" && !contains_bytes(&data, b"GLIBC_2.34") {
            return true;
        }
        if DRIVER_FAMILIES
            .iter()
            .any(|f| contains_bytes(&data, f.as_bytes()))
        {
            return true;
        }
    }
    false
}

/// Substring search anchored on the first byte.
///
/// `windows().any()` compares at every offset. Every needle here starts
/// with `l` or `G`, which is a small fraction of a binary, so seeking the
/// first byte and comparing only there does far less work on the hundreds
/// of megabytes this walks.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    let mut i = 0;
    while let Some(off) = haystack[i..].iter().position(|&b| b == first) {
        let start = i + off;
        let after = start + 1;
        if haystack.len() >= after + rest.len() && &haystack[after..after + rest.len()] == rest {
            return true;
        }
        i = after;
        if i >= haystack.len() {
            break;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("onelf-pack-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A minimal, reproducible PackOptions for tests.
    fn base_opts(dir: &Path, out: &Path, command: &str) -> PackOptions {
        PackOptions {
            directory: dir.to_path_buf(),
            output: out.to_path_buf(),
            command: command.to_string(),
            name: None,
            entrypoints: Vec::new(),
            default_entrypoint: None,
            lib_dirs: Vec::new(),
            level: 3,
            block_size: compress::DEFAULT_BLOCK_SIZE,
            use_dict: false,
            no_compress: false,
            memfd: Some(false),
            working_dir: WorkingDir::Inherit,
            host_libs: HostLibs::default(),
            cache: false,
            update_url: None,
            embed_updater: true,
            update_key: None,
            exclude: Vec::new(),
            package_info: None,
            mtime: Some(0),
            env: Vec::new(),
            preload: Vec::new(),
            needs_setuid: false,
        }
    }

    #[test]
    fn name_round_trips_past_64kib_string_table() {
        let dir = tmpdir("name64k");
        let app = dir.join("app");
        fs::create_dir_all(app.join("bin")).unwrap();
        fs::write(app.join("bin/run"), b"#!/bin/sh\n").unwrap();
        // Push the string table well past 64 KiB with many long unique names
        // so a name added *last* would wrap the u16 offset.
        let pad = "n".repeat(210);
        for i in 0..400 {
            fs::write(app.join(format!("file_{i:04}_{pad}")), b"x").unwrap();
        }

        let out = dir.join("pkg.onelf");
        let mut opts = base_opts(&app, &out, "bin/run");
        opts.name = Some("round-trip-name".to_string());
        pack(&opts, b"stub-runtime").unwrap();

        let (_footer, manifest) = crate::info::read_footer_and_manifest(&out).unwrap();
        assert!(
            manifest.string_table.len() > 0x1_0000,
            "test must exceed a 64 KiB string table, got {}",
            manifest.string_table.len()
        );
        assert_eq!(manifest.name(), "round-trip-name");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_entrypoint_uses_declared_path_and_args_without_duplicate() {
        let dir = tmpdir("default-ep");
        let app = dir.join("app");
        fs::create_dir_all(app.join("bin")).unwrap();
        fs::write(app.join("bin/a"), b"#!/bin/sh\necho a\n").unwrap();
        fs::write(app.join("bin/b"), b"#!/bin/sh\necho b\n").unwrap();

        let out = dir.join("pkg.onelf");
        let mut opts = base_opts(&app, &out, "bin/a");
        opts.entrypoints = vec![(
            "b".to_string(),
            "bin/b".to_string(),
            vec!["--flag".to_string(), "x".to_string()],
        )];
        opts.default_entrypoint = Some("b".to_string());
        pack(&opts, b"stub-runtime").unwrap();

        let (_footer, manifest) = crate::info::read_footer_and_manifest(&out).unwrap();
        // Exactly one entrypoint (no duplicate "b").
        assert_eq!(manifest.entrypoints.len(), 1);
        let ep = &manifest.entrypoints[0];
        assert_eq!(manifest.get_string(ep.name), "b");
        // The default targets bin/b with its declared args, not bin/a/empty.
        assert_eq!(manifest.entry_path(ep.target_entry as usize), "bin/b");
        assert_eq!(manifest.get_string(ep.args), "--flag\x1fx");
        assert_eq!(manifest.header.default_entrypoint, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_utf8_file_name_errors_instead_of_panicking() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tmpdir("nonutf8");
        let app = dir.join("app");
        fs::create_dir_all(app.join("bin")).unwrap();
        fs::write(app.join("bin/run"), b"#!/bin/sh\n").unwrap();
        let bad = std::ffi::OsStr::from_bytes(b"bad-\xff\xfe-name");
        fs::write(app.join(bad), b"x").unwrap();

        let out = dir.join("pkg.onelf");
        let opts = base_opts(&app, &out, "bin/run");
        let err = pack(&opts, b"stub-runtime").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let _ = fs::remove_dir_all(&dir);
    }
}
