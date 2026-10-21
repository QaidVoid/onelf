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
use crate::payload::{ONELF_ENV_SONAME, ONELF_ENV32_SONAME};
use onelf_format::HostLibsPolicy;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
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

/// What a package's payload is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Payload {
    /// Each file cut into blocks that are compressed one by one.
    #[default]
    Blocks,
    /// A stipplefs image, deduplicated and compressed across files.
    Stipple,
}

impl Payload {
    /// The block size a package of this payload gets unless it chooses.
    pub fn default_block_size(self) -> u64 {
        match self {
            Payload::Blocks => compress::DEFAULT_BLOCK_SIZE,
            Payload::Stipple => compress::DEFAULT_STIPPLE_BLOCK_SIZE,
        }
    }

    /// The largest block size a package of this payload may choose.
    fn max_block_size(self) -> u64 {
        match self {
            Payload::Blocks => compress::MAX_BLOCK_SIZE,
            Payload::Stipple => compress::MAX_STIPPLE_BLOCK_SIZE,
        }
    }
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
    /// What the payload is made of. See [`Payload`].
    pub payload: Payload,
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
    /// Ask the runtime to run from the persistent cache before trying any
    /// other execution mode. Implies [`PackOptions::cache`].
    pub prefer_cache: bool,
    pub update_url: Option<String>,
    /// Embed the update-capable runtime when `update_url` is set. False
    /// records the update metadata but links the slim runtime, for
    /// packages updated by a package manager rather than by themselves.
    pub embed_updater: bool,
    /// Raw Ed25519 public key bytes embedded as `.onelf/update-key`; the
    /// runtime requires at least one key (and a valid signature) before
    /// self-updating. The length must be a multiple of 32: one key is the
    /// classic case, several concatenated keys form a keyring, and the
    /// runtime accepts an update signature made with any key in it.
    pub update_key: Option<Vec<u8>>,
    /// Monotonic revision embedded as `.onelf/update-revision`; the
    /// runtime refuses an update whose revision is not newer, so a
    /// rolled-back build cannot be reinstalled. The detached signature
    /// covers it.
    pub update_revision: Option<u64>,
    pub exclude: Vec<String>,
    /// Optional TOML-formatted metadata written to `.onelf/package-info.toml`.
    pub package_info: Option<String>,
    /// Pin every entry's mtime to this Unix timestamp (nsec 0) for fully
    /// reproducible output independent of filesystem timestamps.
    pub mtime: Option<u64>,
    /// Custom environment variables to set before exec (KEY=VALUE pairs).
    pub env: Vec<(String, String)>,
    /// Absolute host path to bundle-relative directory, for an
    /// application that opens a path it was built with.
    pub paths: Vec<(String, String)>,
    /// Libraries to dlopen on every exec, written to `.onelf/preload` and
    /// applied by the bundled onelf-env constructor. `${ONELF_DIR}`
    /// expands to the package root at runtime.
    pub preload: Vec<String>,
    /// Whether the app runs setuid binaries, written as `.onelf/needs-setuid`
    /// and read by the runtime before it picks how to unpack itself. Keeps the
    /// app out of a user namespace, where a setuid bit does nothing.
    pub needs_setuid: bool,
    /// Blocks a startup reads, as `(path, offset)` in the order it first
    /// read them, from [`read_profile`]. Packed as `.onelf/prefetch` for the
    /// runtime to decompress ahead of the app.
    pub profile: Vec<(String, u64)>,
}

/// Where the runtime looks for the blocks to prefetch.
const PREFETCH_FILE: &str = ".onelf/prefetch";

/// Read a profile recorded with `ONELF_PROFILE`: one `path<TAB>offset` line
/// per block, in the order the run first read them. Blank lines and `#`
/// comments are skipped.
pub fn read_profile(path: &Path) -> io::Result<Vec<(String, u64)>> {
    let text = fs::read_to_string(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    parse_profile(&text).map_err(|(line, what)| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}:{line}: {what}", path.display()),
        )
    })
}

fn parse_profile(text: &str) -> Result<Vec<(String, u64)>, (usize, &'static str)> {
    let mut blocks = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let (path, offset) = line
            .split_once('\t')
            .ok_or((n + 1, "expected a path and an offset separated by a tab"))?;
        let offset = offset
            .trim()
            .parse::<u64>()
            .map_err(|_| (n + 1, "the offset is not a number"))?;
        blocks.push((path.to_string(), offset));
    }
    Ok(blocks)
}

/// The `.onelf/prefetch` lines for `profile`: `path<TAB>block` for each
/// block of `files` it names, in its order, each block once. A profile
/// taken with another block size still lands on the block holding each
/// offset, and a path this tree does not have is left out.
fn prefetch_lines(
    files: &[CollectedFile],
    profile: &[(String, u64)],
    block_size: u64,
) -> Vec<String> {
    let sizes: HashMap<&Path, u64> = files
        .iter()
        .map(|f| (f.rel_path.as_path(), f.size))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for (path, offset) in profile {
        let rel = package_relative(path);
        if !sizes.get(rel.as_path()).is_some_and(|&size| *offset < size) {
            continue;
        }
        let Some(rel) = rel.to_str() else {
            continue;
        };
        let block = offset / block_size;
        if seen.insert((rel.to_string(), block)) {
            lines.push(format!("{rel}\t{block}"));
        }
    }
    lines
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

/// Open a file the walk recorded, refusing what the walk did not see.
///
/// Between the walk and this open the path can be anything: a symlink
/// swapped in over the regular file, a FIFO that would block `open`
/// forever with no writer, a device node that would read forever. The
/// flags refuse the first two and the fstat after open refuses the
/// rest, so what is read through the handle is a regular file, whatever
/// the path now names.
fn open_walked(path: &Path) -> io::Result<std::fs::File> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?;
    let file = std::fs::File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: changed while packing; no longer a regular file",
                path.display()
            ),
        ));
    }
    Ok(file)
}

impl FileSource {
    fn read(&self) -> io::Result<Cow<'_, [u8]>> {
        match self {
            FileSource::Disk(path) => {
                let mut file = open_walked(path)?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)?;
                Ok(Cow::Owned(buf))
            }
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

/// Compile GSettings schemas when the tree ships schema XML without the
/// compiled blob. The bundle flow does the same inside the AppDir; plain
/// `pack` must not fall behind, or an app reading GSettings crashes on its
/// first settings access. The blob is injected as an in-memory entry, so
/// the input tree is never written to. A missing or failing compiler only
/// warns: the app may ship schemas without using GSettings.
fn compile_gsettings_schemas(
    dirs: &[CollectedDir],
    files: &mut Vec<CollectedFile>,
    directory: &Path,
    inject_mtime: u64,
) {
    const SCHEMAS_DIR: &str = "share/glib-2.0/schemas";
    let schemas_rel = Path::new(SCHEMAS_DIR);
    let compiled_rel = schemas_rel.join("gschemas.compiled");
    if !dirs.iter().any(|d| d.rel_path == schemas_rel) {
        return;
    }
    if files.iter().any(|f| f.rel_path == compiled_rel) {
        return;
    }
    let xml_count = files
        .iter()
        .filter(|f| {
            f.rel_path.parent() == Some(schemas_rel)
                && f.rel_path.extension().is_some_and(|e| e == "xml")
        })
        .count();
    if xml_count == 0 {
        return;
    }

    let compiler = crate::bundle::gpu::find_glib_compile_schemas();
    let tmp = match crate::bundle::private_temp_dir("schemas") {
        Ok(tmp) => tmp,
        Err(e) => {
            eprintln!("warning: no scratch directory to compile GSettings schemas in: {e}");
            return;
        }
    };
    let run = std::process::Command::new(&compiler)
        .arg("--targetdir")
        .arg(&tmp)
        .arg(directory.join(schemas_rel))
        .output();
    let blob = match run {
        Ok(out) if out.status.success() => fs::read(tmp.join("gschemas.compiled")),
        Ok(out) => {
            eprintln!(
                "warning: glib-compile-schemas failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            Ok(Vec::new())
        }
        Err(e) => {
            eprintln!(
                "warning: glib-compile-schemas not runnable ({e}); ship a compiled \
                 gschemas.compiled or install glib development tools"
            );
            Ok(Vec::new())
        }
    };
    let _ = fs::remove_dir_all(&tmp);
    match blob {
        Ok(bytes) if !bytes.is_empty() => {
            eprintln!(
                "  Compiled GSettings schemas ({} bytes, {} sources)",
                bytes.len(),
                xml_count
            );
            let size = bytes.len() as u64;
            files.push(CollectedFile {
                rel_path: compiled_rel,
                source: FileSource::Memory(bytes),
                size,
                mode: 0o644,
                mtime_secs: inject_mtime,
                mtime_nsec: 0,
            });
        }
        Ok(_) => {}
        Err(e) => eprintln!("warning: could not read compiled schemas: {e}"),
    }
}

/// A path as the walk records it: `./bin/app` and `bin/app` name the same
/// entry, and a shell completion or a recipe writes either.
fn package_relative(path: &str) -> PathBuf {
    Path::new(path)
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// A scratch file beside `output` for this pack alone. The whole output
/// name is kept, so `app.x86_64` and `app.aarch64` packing at once in one
/// directory do not share it, and the pid is added, so two packs of one
/// output do not either.
fn sibling_tmp(output: &Path, tag: &str) -> PathBuf {
    let mut name = output.as_os_str().to_os_string();
    name.push(format!(".onelf-{tag}-{}.tmp", std::process::id()));
    PathBuf::from(name)
}

/// True when `path`, `len` bytes long, is itself a onelf package: the
/// runtime's ELF header carries the `ONELF` signature and the file ends in
/// the footer's magic. An earlier output left in a recipe's own directory
/// is one, under whatever name it was built with.
fn is_onelf_package(path: &Path, len: u64) -> bool {
    use std::io::{Seek, SeekFrom};
    if len < onelf_format::FOOTER_SIZE as u64 {
        return false;
    }
    let Ok(mut f) = open_walked(path) else {
        return false;
    };
    let mut head = [0u8; 16];
    if f.read_exact(&mut head).is_err() || head[0..4] != *b"\x7fELF" || head[9..15] != *b"ONELF\0" {
        return false;
    }
    let mut tail = [0u8; 8];
    f.seek(SeekFrom::End(-8)).is_ok()
        && f.read_exact(&mut tail).is_ok()
        && tail == onelf_format::END_MAGIC
}

/// Scratch files beside the output, removed when the pack ends, however it
/// ends. The payload alone can be gigabytes.
struct Scratch(Vec<PathBuf>);

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// True for a name [`sibling_tmp`] produces, whichever pack produced it.
fn is_scratch_name(name: &str) -> bool {
    name.ends_with(".tmp") && name.contains(".onelf-")
}

pub fn pack(opts: &PackOptions, runtime_binary: &[u8]) -> io::Result<()> {
    // The CLI parser enforces this range; a recipe does not, and a block
    // size of zero never advances the chunker.
    let max_block_size = opts.payload.max_block_size();
    if !(compress::MIN_BLOCK_SIZE..=max_block_size).contains(&opts.block_size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "block size {} is outside {}..={} bytes",
                opts.block_size,
                compress::MIN_BLOCK_SIZE,
                max_block_size
            ),
        ));
    }
    if opts.payload == Payload::Stipple {
        if !opts.block_size.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a stipplefs payload needs a power-of-two block size, got {}",
                    opts.block_size
                ),
            ));
        }
        if opts.use_dict {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--dict trains a dictionary for a blocks payload; a stipplefs payload trains its own",
            ));
        }
    }
    if let Some(ref url) = opts.update_url
        && !url.starts_with("https://")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("update URL must use https://, got {url}"),
        ));
    }
    if let Some(ref key) = opts.update_key {
        check_update_key(key)?;
    }
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
    // Entries the walk silently passes over: kinds the package format has
    // no representation for, and scratch names that may or may not be
    // crashed-pack leftovers. Both are reported after the walk.
    let mut dropped_special: Vec<(PathBuf, &'static str)> = Vec::new();
    let mut dropped_scratch: Vec<PathBuf> = Vec::new();
    let mut dropped_packages: Vec<PathBuf> = Vec::new();

    // The output of a previous pack may sit inside the directory, as it
    // does for a recipe whose AppDir is the recipe's own directory. It is
    // never content.
    let dir_abs = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let output_abs = opts.output.parent().and_then(|parent| {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        Some(parent.canonicalize().ok()?.join(opts.output.file_name()?))
    });

    for entry in WalkDir::new(&dir).skip_hidden(false).sort(true) {
        let entry = entry.map_err(io::Error::other)?;
        let abs_path = entry.path();
        let rel_path = abs_path.strip_prefix(&dir).unwrap().to_path_buf();

        if rel_path.as_os_str().is_empty() {
            continue;
        }
        if output_abs.as_deref() == Some(dir_abs.join(&rel_path).as_path()) {
            continue;
        }
        // The packer's own bookkeeping of a sysroot build, not content.
        if rel_path == Path::new(crate::bundle::sysroot::GENERATED_FILE) {
            continue;
        }
        // Scratch a crashed pack left beside its output, not content either.
        if is_scratch_name(&entry.file_name().to_string_lossy()) {
            dropped_scratch.push(rel_path);
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
            if is_onelf_package(&abs_path, symlink_meta.len()) {
                dropped_packages.push(rel_path);
                continue;
            }
            files.push(CollectedFile {
                rel_path,
                source: FileSource::Disk(abs_path.clone()),
                size: symlink_meta.len(),
                mode,
                mtime_secs,
                mtime_nsec,
            });
        } else {
            use std::os::unix::fs::FileTypeExt;
            let ft = symlink_meta.file_type();
            let kind = if ft.is_fifo() {
                "fifo"
            } else if ft.is_socket() {
                "socket"
            } else if ft.is_char_device() {
                "char device"
            } else if ft.is_block_device() {
                "block device"
            } else {
                "special file"
            };
            dropped_special.push((rel_path, kind));
        }
    }

    for (path, kind) in dropped_special.iter().take(10) {
        eprintln!(
            "warning: dropping {kind} {} (not representable in a package)",
            path.display()
        );
    }
    if dropped_special.len() > 10 {
        eprintln!(
            "warning: dropping {} more special files (not representable in a package)",
            dropped_special.len() - 10
        );
    }
    for path in dropped_scratch.iter().take(10) {
        eprintln!(
            "warning: dropping {} (scratch name left by a crashed pack; rename it if it is content)",
            path.display()
        );
    }
    if dropped_scratch.len() > 10 {
        eprintln!(
            "warning: dropping {} more scratch-named files",
            dropped_scratch.len() - 10
        );
    }
    for path in &dropped_packages {
        eprintln!(
            "warning: dropping {} (a onelf package, such as an earlier output)",
            path.display()
        );
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
                && p.as_path() != Path::new(crate::bundle::sysroot::PLATFORM_FILE)
                && p.as_path() != Path::new(crate::bundle::sysroot::SETS_FILE)
                && p.as_path() != Path::new(crate::bundle::sysroot::SHIPPED_FILE)
                && p.as_path() != Path::new(crate::bundle::sysroot::PATHS_FILE)
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

    // Inject .onelf/update-revision if requested. The detached signature
    // covers the whole package, so this number is authenticated, and the
    // runtime refuses an update that is not newer than what it runs.
    if let Some(rev) = opts.update_revision {
        inject_onelf_file(
            &mut dirs,
            &mut files,
            ".onelf/update-revision",
            rev.to_string().into_bytes(),
            inject_mtime,
        );
    }

    // Inject .onelf/update-key (raw Ed25519 public keys) if provided.
    // One key is the common case; a concatenation of N keys is a keyring
    // and the runtime accepts a signature from any of them.
    if let Some(ref key) = opts.update_key {
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
    // Compile GSettings schemas when only the XML sources are shipped.
    compile_gsettings_schemas(&dirs, &mut files, &opts.directory, inject_mtime);

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

    // Write the path overlays as .onelf/paths (ABSOLUTE\tRELATIVE per
    // line). The runtime makes each bundle directory answer for the
    // absolute path, for an application that opens one it was built
    // with and offers no way to say otherwise.
    if !opts.paths.is_empty() {
        let lines: Vec<String> = opts
            .paths
            .iter()
            .map(|(abs, rel)| format!("{abs}\t{rel}"))
            .collect();
        inject_onelf_file(
            &mut dirs,
            &mut files,
            crate::bundle::sysroot::PATHS_FILE,
            lines.join("\n").into_bytes(),
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

    // Last, so every file the profile may name has been collected.
    if !opts.profile.is_empty() {
        let unit = match opts.payload {
            Payload::Blocks => opts.block_size,
            Payload::Stipple => onelf_format::payload::SLICE_SIZE,
        };
        let lines = prefetch_lines(&files, &opts.profile, unit);
        if lines.is_empty() {
            eprintln!("warning: the profile names no block of this tree; nothing to prefetch");
        } else {
            eprintln!("  Prefetch: {} blocks from the profile", lines.len());
            inject_onelf_file(
                &mut dirs,
                &mut files,
                PREFETCH_FILE,
                lines.join("\n").into_bytes(),
                inject_mtime,
            );
        }
    }

    pb.finish_with_message(format!(
        "Found {} dirs, {} files, {} symlinks",
        dirs.len(),
        files.len(),
        symlinks.len()
    ));

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

    // A default naming no declared entrypoint used to fall back to the
    // package command under the typed name, silently producing something the
    // recipe never described.
    if let Some(name) = opts.default_entrypoint.as_deref()
        && !opts.entrypoints.iter().any(|(n, _, _)| n == name)
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("default entrypoint '{name}' matches no declared entrypoint"),
        ));
    }

    // Every path the entrypoints name is in the tree. Checked before
    // compressing, which for a large tree is most of the run.
    {
        let walked: std::collections::HashSet<&Path> = files
            .iter()
            .map(|f| f.rel_path.as_path())
            .chain(dirs.iter().map(|d| d.rel_path.as_path()))
            .chain(symlinks.iter().map(|s| s.rel_path.as_path()))
            .collect();
        if !walked.contains(package_relative(&opts.command).as_path()) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("command path '{}' not found in directory", opts.command),
            ));
        }
        if let Some((_, path, _)) = opts
            .entrypoints
            .iter()
            .find(|(_, p, _)| !walked.contains(package_relative(p).as_path()))
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("entrypoint path '{}' not found in directory", path),
            ));
        }
    }

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

    let payload_tmp_path = sibling_tmp(&opts.output, "payload");
    let staged = sibling_tmp(&opts.output, "staged");
    let _scratch = Scratch(vec![payload_tmp_path.clone(), staged.clone()]);
    let mut payload_tmp = BufWriter::new(File::create(&payload_tmp_path)?);
    let mut payload_offset: u64 = 0;
    let mut compressed_files: Vec<CompressedFile> = Vec::with_capacity(files.len());
    // Identical content is written once and shared, so a tree with repeated
    // files pays for them once rather than per path.
    let mut seen: HashMap<[u8; 32], Vec<onelf_format::Block>> = HashMap::new();
    let mut deduped_bytes: u64 = 0;

    let mut blocks_payload = || -> io::Result<()> {
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
    };
    let result = match opts.payload {
        Payload::Blocks => blocks_payload(),
        Payload::Stipple => stipple_payload(&files, opts, &mut payload_tmp, &payload_tmp_path, &pb)
            .map(|(built, size)| {
                compressed_files = built;
                payload_offset = size;
            }),
    };
    drop(payload_tmp);
    result?;

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
    let command_path = package_relative(&opts.command);
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

    // When `default_entrypoint` names a declared `[[entrypoint]]`, the
    // default launch must use that entrypoint's path and args, not the
    // package command's, and the tuple must not also be appended below as a
    // second entrypoint with the same name.
    let default_decl = opts
        .default_entrypoint
        .as_deref()
        .and_then(|dn| opts.entrypoints.iter().find(|(n, _, _)| n == dn));

    let ep_name = strings.add(default_name);
    let empty_args = strings.add("");

    let (ep0_target_idx, ep0_target_path) = match default_decl {
        Some((_, path, _)) => {
            let p = package_relative(path);
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
        let ep_path = package_relative(path);
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

    // Patch ELF header with ONELF signature in e_ident padding (bytes 9-14)
    let mut runtime_patched = runtime_binary.to_vec();
    if runtime_patched.len() >= 16 {
        // Bytes 9-14 are EI_PAD (padding), we can use them for signature
        // Signature: "ONELF\x00" (6 bytes)
        runtime_patched[9..15].copy_from_slice(b"ONELF\x00");
    }

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
            // Container structure hashes, format version 2: the
            // manifest pins the runtime prefix and (once its bytes exist)
            // the footer, so `verify` can check both.
            prefix_hash: blake3::hash(&runtime_patched).into(),
            footer_hash: [0; 32], // placeholder, patched below
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

    // Compute manifest checksum (xxhash32 of uncompressed manifest, with
    // the footer_hash field excluded; it is pinned by the footer hash
    // instead, see Footer::structure_hash).
    let manifest_checksum = onelf_format::manifest::manifest_checksum(&manifest_bytes)?;

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
    match opts.payload {
        Payload::Blocks if opts.no_compress => flags |= Flags::STORED,
        Payload::Blocks => {}
        Payload::Stipple => flags |= Flags::STIPPLE_PAYLOAD,
    }
    if opts.update_url.is_some() && !opts.embed_updater {
        flags |= Flags::EXTERNAL_UPDATER;
    }
    let policy = match opts.host_libs {
        HostLibs::Always => HostLibsPolicy::Always,
        HostLibs::Never => HostLibsPolicy::Never,
        HostLibs::Auto if bundle_needs_host_libs(&files) => HostLibsPolicy::Auto,
        HostLibs::Auto => HostLibsPolicy::Never,
    };
    flags |= policy.to_flags();
    if opts.cache || opts.prefer_cache {
        flags |= Flags::CACHE_REQUESTED;
    }
    if opts.prefer_cache {
        flags |= Flags::CACHE_PREFERRED;
    }

    // Write the output file
    let pb = ProgressBar::new_spinner();
    pb.set_message("Writing output...");

    let runtime_size = runtime_binary.len() as u64;
    let manifest_offset = runtime_size;
    let dict_size = dict.as_ref().map(|d| d.len() as u32).unwrap_or(0);

    // The footer is hashed into the manifest, and the manifest's
    // compressed size is only known after that hash is in, so the hash
    // covers the footer with the three manifest-derived size fields
    // zeroed (Footer::structure_hash). They are filled in afterwards,
    // from the final compressed manifest.
    let mut footer = Footer {
        format_version: onelf_format::footer::FORMAT_VERSION,
        flags,
        manifest_offset,
        manifest_compressed: 0,
        manifest_original: 0,
        payload_offset: 0,
        payload_size: total_payload,
        dict_offset: 0,
        dict_size,
        manifest_checksum,
    };
    let footer_hash = footer.structure_hash();
    manifest_bytes[82..114].copy_from_slice(&footer_hash);

    // Compress manifest
    let manifest_compressed = compress::compress_manifest(&manifest_bytes)?;
    footer.manifest_compressed = manifest_compressed.len() as u64;
    footer.manifest_original = manifest_bytes.len() as u64;
    footer.payload_offset = manifest_offset + manifest_compressed.len() as u64;
    if dict.is_some() {
        footer.dict_offset = footer.payload_offset + total_payload;
    }

    // Written beside the output and renamed over it at the end, so a
    // reader never sees a partial package and an instance still running
    // the previous one keeps its file.
    let out = File::create(&staged)?;
    let mut w = BufWriter::new(out);

    // [Runtime ELF], already patched above and hashed into the manifest
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
    fs::set_permissions(&staged, perms)?;
    fs::rename(&staged, &opts.output)?;

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
    if opts.payload == Payload::Stipple {
        eprintln!(
            "  {} {} (stipplefs image, {} blocks, {})",
            bold("Payload:"),
            format_size(total_payload),
            format_size(opts.block_size),
            if opts.no_compress {
                "stored".to_string()
            } else {
                format!("zstd level {}", opts.level)
            }
        );
    } else if opts.no_compress {
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

/// Build the payload as a stipplefs image, written through `out` to
/// `out_path`, then read every file back out of it to record its slices.
///
/// The slices are taken from the image rather than from the source, so the
/// manifest describes exactly what the image holds even if a file changed
/// after the walk. They are read in the image's own order, which decodes
/// each of its blocks once.
fn stipple_payload(
    files: &[CollectedFile],
    opts: &PackOptions,
    out: &mut BufWriter<File>,
    out_path: &Path,
    pb: &ProgressBar,
) -> io::Result<(Vec<CompressedFile>, u64)> {
    use std::os::unix::ffi::OsStrExt;
    use stipplefs::format::Codec;
    use stipplefs::write::{Builder, FileOrder, Node, Options, Timestamps};

    let stipple_error = |e: stipplefs::error::Error| io::Error::other(format!("stipplefs: {e}"));
    let codecs = if opts.no_compress {
        Codec::Stored.bit()
    } else {
        Codec::Stored.bit()
            | Codec::Zstd.bit()
            | Codec::ZstdDict.bit()
            | Codec::ZstdBcj.bit()
            | Codec::ZstdDictBcj.bit()
    };
    // What mkstipple chooses, apart from the times: the manifest records
    // every entry's, so the image's are never read.
    let mut builder = Builder::new(Options {
        block_size: opts.block_size as u32,
        chunk_average: 4096,
        codecs,
        level: opts.level,
        dictionary_size: if opts.no_compress || opts.block_size > 1 << 20 {
            0
        } else {
            112 * 1024
        },
        threads: rayon::current_num_threads(),
        memory_limit: None,
        timestamps: Timestamps::Clamp,
        compress_metadata: !opts.no_compress,
    })
    .map_err(stipple_error)?;
    builder.set_order(FileOrder::Extension);
    builder.set_hot_list(
        opts.profile
            .iter()
            .map(|(path, _)| package_relative(path).into_os_string().into_encoded_bytes()),
    );
    for f in files {
        let path = f.rel_path.as_os_str().as_bytes();
        let mode = f.mode & 0o7777;
        match &f.source {
            FileSource::Disk(source) => builder.insert_file_at(path, source, mode, 0, 0),
            FileSource::Memory(content) => builder.insert(
                path,
                Node::File {
                    content: content.clone(),
                    mode,
                    uid: 0,
                    gid: 0,
                    link_group: None,
                },
            ),
        }
        .map_err(stipple_error)?;
    }
    pb.set_message("Building the stipplefs image...");
    builder.build_into(out).map_err(stipple_error)?;
    out.flush()?;

    pb.set_message("Hashing files...");
    let image = stipplefs::read::Image::open(out_path).map_err(stipple_error)?;
    let reader = image.reader().map_err(stipple_error)?;
    let inodes = files
        .iter()
        .map(|f| {
            reader
                .resolve(f.rel_path.as_os_str().as_bytes())
                .ok_or_else(|| {
                    io::Error::other(format!("{}: not in the image", f.rel_path.display()))
                })
        })
        .collect::<io::Result<Vec<u32>>>()?;
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_cached_key(|&i| {
        reader
            .file_extents(inodes[i])
            .first()
            .map_or(u64::MAX, |e| e.0)
    });

    let mut built: Vec<Option<CompressedFile>> = files.iter().map(|_| None).collect();
    let mut buf = vec![0u8; onelf_format::payload::SLICE_SIZE as usize];
    for i in order {
        let (f, ino) = (&files[i], inodes[i]);
        let size = reader.inode(ino).map_or(0, |n| n.size);
        let mut whole = blake3::Hasher::new();
        let mut blocks = Vec::new();
        let mut offset = 0;
        while offset < size {
            let n = reader.read(ino, offset, &mut buf).map_err(stipple_error)?;
            if n == 0 {
                return Err(io::Error::other(format!(
                    "{}: the image ends the file early",
                    f.rel_path.display()
                )));
            }
            let slice = &buf[..n];
            whole.update(slice);
            blocks.push(onelf_format::Block {
                payload_offset: offset,
                compressed_size: ino as u64,
                original_size: n as u64,
                content_hash: *blake3::hash(slice).as_bytes(),
            });
            offset += n as u64;
        }
        built[i] = Some(CompressedFile {
            rel_path: f.rel_path.clone(),
            blocks,
            content_hash: *whole.finalize().as_bytes(),
            mode: f.mode,
            mtime_secs: f.mtime_secs,
            mtime_nsec: f.mtime_nsec,
        });
        pb.inc(1);
    }
    let size = fs::metadata(out_path)?.len();
    Ok((built.into_iter().flatten().collect(), size))
}

/// Refuses an update key file that is not a ring of distinct 32-byte
/// public keys. A 64-byte Ed25519 secret key is a seed followed by its
/// public key, so it has a valid ring length and would otherwise ship
/// the private seed inside every package.
fn check_update_key(key: &[u8]) -> io::Result<()> {
    use ed25519_compact::{KeyPair, Seed};

    let invalid = |msg: String| Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
    if key.is_empty() || !key.len().is_multiple_of(32) {
        return invalid(format!(
            "update key must be one or more 32-byte Ed25519 public keys \
             (a multiple of 32 bytes), got {} bytes",
            key.len()
        ));
    }
    let (chunks, _) = key.as_chunks::<32>();
    for (i, pair) in chunks.windows(2).enumerate() {
        if *KeyPair::from_seed(Seed::new(pair[0])).pk == pair[1] {
            return invalid(format!(
                "update key bytes {}..{} look like an Ed25519 secret key, which \
                 must never ship in a package; pass the public key from \
                 `onelf key new` instead",
                i * 32,
                i * 32 + 64
            ));
        }
    }
    for (i, chunk) in chunks.iter().enumerate() {
        if chunks[..i].contains(chunk) {
            return invalid(format!("update key {} repeats an earlier key", i + 1));
        }
    }
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

/// `SOURCE_DATE_EPOCH` as whole seconds. A value that is not one is
/// treated as unset everywhere it is read, and said so once.
pub(crate) fn source_date_epoch() -> Option<u64> {
    let value = std::env::var("SOURCE_DATE_EPOCH").ok()?;
    let parsed = value.trim().parse().ok();
    if parsed.is_none() {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!(
                "warning: SOURCE_DATE_EPOCH={value:?} is not a whole number of \
                 seconds, so it is ignored"
            )
        });
    }
    parsed
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
        Ok(elf) => elf
            .libraries
            .iter()
            .any(|l| *l == ONELF_ENV_SONAME || *l == ONELF_ENV32_SONAME),
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

/// Whether the files being packed need libraries the host must supply.
///
/// Deliberately generous. Guessing "no" when the answer is "yes" breaks an
/// app that works today, while guessing "yes" only leaves the current
/// behaviour in place, so anything that looks like a host-provided family
/// counts.
///
/// Driver stacks are loaded by `dlopen` at runtime and so never appear in
/// `DT_NEEDED`. Their sonames are looked for as strings anywhere in the
/// bundle's ELF files instead. Only what is packed is consulted: a file
/// `--exclude` left out cannot load anything.
fn bundle_needs_host_libs(files: &[CollectedFile]) -> bool {
    use onelf_format::drivers::DRIVER_FAMILIES;

    for file in files {
        // A pinned GL build or a shared set says outright that something
        // comes from outside the bundle. Both are reached through the
        // launch resolver, so a package carrying either needs it, even
        // when nothing left in the bundle names a driver family: the
        // libraries that did were the ones left out.
        let rel = file.rel_path.as_path();
        if rel == Path::new(crate::bundle::sysroot::PLATFORM_FILE)
            || rel == Path::new(crate::bundle::sysroot::SETS_FILE)
        {
            return true;
        }
        let name = file
            .rel_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if DRIVER_FAMILIES.iter().any(|f| name.contains(f)) {
            return true;
        }
        let data = match &file.source {
            // Magic first. A tree like Blender's is mostly Python and data
            // files, and reading those in full to discover they are not ELF
            // costs more than the whole rest of this scan.
            FileSource::Disk(path) => {
                let Ok(mut f) = open_walked(path) else {
                    continue;
                };
                let mut magic = [0u8; 4];
                if std::io::Read::read_exact(&mut f, &mut magic).is_err() || magic != *b"\x7fELF" {
                    continue;
                }
                let mut data = magic.to_vec();
                if std::io::Read::read_to_end(&mut f, &mut data).is_err() {
                    continue;
                }
                Cow::Owned(data)
            }
            FileSource::Memory(bytes) => {
                if !bytes.starts_with(b"\x7fELF") {
                    continue;
                }
                Cow::Borrowed(bytes.as_slice())
            }
        };
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

    /// The walk saw a regular file; by read time the path is anything.
    /// The open must refuse the impostors fast and read the genuine one.
    #[test]
    fn a_swapped_in_path_is_refused_not_hung() {
        let dir = tmpdir("toctou");
        let real = dir.join("real.bin");
        fs::write(&real, b"content").unwrap();

        // A symlink swapped in over the file: O_NOFOLLOW refuses it.
        let link = dir.join("link.bin");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(open_walked(&link).is_err(), "a symlink must be refused");

        // A socket standing where the file was: the open must fail fast,
        // exactly like any other non-regular file, not block or spin.
        let sock = dir.join("sock.bin");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(open_walked(&sock).is_err(), "a socket must be refused");

        // The genuine article still reads through the handle.
        let mut f = open_walked(&real).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"content");

        fs::remove_dir_all(&dir).unwrap();
    }

    /// A minimal, reproducible PackOptions for tests.
    fn base_opts(dir: &Path, out: &Path, command: &str) -> PackOptions {
        PackOptions {
            directory: dir.to_path_buf(),
            output: out.to_path_buf(),
            command: command.to_string(),
            name: None,
            paths: Vec::new(),
            entrypoints: Vec::new(),
            default_entrypoint: None,
            lib_dirs: Vec::new(),
            level: 3,
            block_size: compress::DEFAULT_BLOCK_SIZE,
            payload: Payload::Blocks,
            use_dict: false,
            no_compress: false,
            memfd: Some(false),
            working_dir: WorkingDir::Inherit,
            host_libs: HostLibs::default(),
            cache: false,
            prefer_cache: false,
            update_url: None,
            embed_updater: true,
            update_key: None,
            update_revision: None,
            exclude: Vec::new(),
            package_info: None,
            mtime: Some(0),
            env: Vec::new(),
            preload: Vec::new(),
            needs_setuid: false,
            profile: Vec::new(),
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

    const SCHEMA_XML: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<schemalist>\n",
        "  <schema id=\"com.example.pack-test\" path=\"/com/example/pack-test/\">\n",
        "    <key name=\"greeting\" type=\"s\">\n",
        "      <default>'hi'</default>\n",
        "      <summary>Greeting</summary>\n",
        "    </key>\n",
        "  </schema>\n",
        "</schemalist>\n",
    );

    /// A tree holding only a schema dir with one XML source, plus the walk
    /// results that would correspond to it.
    fn schemas_tree(tag: &str) -> (PathBuf, Vec<CollectedDir>, Vec<CollectedFile>) {
        let dir = tmpdir(tag);
        let schemas = dir.join("share/glib-2.0/schemas");
        fs::create_dir_all(&schemas).unwrap();
        fs::write(
            schemas.join("com.example.pack-test.gschema.xml"),
            SCHEMA_XML,
        )
        .unwrap();
        let dirs = vec![CollectedDir {
            rel_path: PathBuf::from("share/glib-2.0/schemas"),
            mode: 0o755,
            mtime_secs: 0,
            mtime_nsec: 0,
        }];
        let files = vec![CollectedFile {
            rel_path: PathBuf::from("share/glib-2.0/schemas/com.example.pack-test.gschema.xml"),
            source: FileSource::Disk(schemas.join("com.example.pack-test.gschema.xml")),
            size: SCHEMA_XML.len() as u64,
            mode: 0o644,
            mtime_secs: 0,
            mtime_nsec: 0,
        }];
        (dir, dirs, files)
    }

    fn glib_tool_available() -> bool {
        std::process::Command::new("which")
            .arg("glib-compile-schemas")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn pack_compiles_missing_gsettings_schemas() {
        if !glib_tool_available() {
            eprintln!("skip: glib-compile-schemas not available");
            return;
        }
        let (dir, dirs, mut files) = schemas_tree("compile");
        compile_gsettings_schemas(&dirs, &mut files, &dir, 0);

        let blob = files
            .iter()
            .find(|f| f.rel_path == Path::new("share/glib-2.0/schemas/gschemas.compiled"))
            .expect("compiled schemas entry injected");
        let FileSource::Memory(bytes) = &blob.source else {
            panic!("compiled schemas must be an in-memory entry");
        };
        assert!(!bytes.is_empty());
        assert!(
            !dir.join("share/glib-2.0/schemas/gschemas.compiled")
                .exists()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pack_keeps_shipped_gsettings_blob() {
        if !glib_tool_available() {
            eprintln!("skip: glib-compile-schemas not available");
            return;
        }
        let (dir, dirs, mut files) = schemas_tree("shipped");
        let sentinel = b"packager-provided blob".to_vec();
        let blob_path = dir.join("share/glib-2.0/schemas/gschemas.compiled");
        fs::write(&blob_path, &sentinel).unwrap();
        files.push(CollectedFile {
            rel_path: PathBuf::from("share/glib-2.0/schemas/gschemas.compiled"),
            source: FileSource::Disk(blob_path.clone()),
            size: sentinel.len() as u64,
            mode: 0o644,
            mtime_secs: 0,
            mtime_nsec: 0,
        });

        compile_gsettings_schemas(&dirs, &mut files, &dir, 0);

        assert_eq!(files.len(), 2, "no entry injected past the shipped blob");
        assert_eq!(fs::read(&blob_path).unwrap(), sentinel);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pack_ignores_schema_dir_without_xml() {
        let (dir, dirs, mut files) = schemas_tree("noxml");
        files.clear();
        let readme = dir.join("share/glib-2.0/schemas/README");
        fs::write(&readme, b"not a schema").unwrap();
        files.push(CollectedFile {
            rel_path: PathBuf::from("share/glib-2.0/schemas/README"),
            source: FileSource::Disk(readme),
            size: 12,
            mode: 0o644,
            mtime_secs: 0,
            mtime_nsec: 0,
        });

        compile_gsettings_schemas(&dirs, &mut files, &dir, 0);
        assert!(files.len() == 1, "nothing injected without schema XML");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A 64-byte Ed25519 secret key is a seed followed by its public key,
    /// which has a valid ring length. It must be refused so the seed never
    /// ships in a package.
    #[test]
    fn update_key_refuses_a_secret_key_and_repeats() {
        use ed25519_compact::{KeyPair, Seed};
        let a = KeyPair::from_seed(Seed::new([7u8; 32]));
        let b = KeyPair::from_seed(Seed::new([9u8; 32]));

        let err = check_update_key(&a.sk[..]).unwrap_err().to_string();
        assert!(err.contains("secret key"), "{err}");
        let ring_with_secret = [&b.pk[..], &a.sk[..]].concat();
        assert!(check_update_key(&ring_with_secret).is_err());
        let repeated = [&a.pk[..], &b.pk[..], &a.pk[..]].concat();
        let err = check_update_key(&repeated).unwrap_err().to_string();
        assert!(err.contains("repeats"), "{err}");

        check_update_key(&a.pk[..]).unwrap();
        check_update_key(&[&a.pk[..], &b.pk[..]].concat()).unwrap();
        assert!(check_update_key(&[]).is_err());
        assert!(check_update_key(&[0u8; 33]).is_err());
    }

    /// Every client refuses a non-HTTPS update URL, so packing one would
    /// ship a package that can never update.
    #[test]
    fn plain_http_update_url_is_refused() {
        let dir = tmpdir("http-url");
        let app = dir.join("app");
        fs::create_dir_all(app.join("bin")).unwrap();
        fs::write(app.join("bin/run"), b"#!/bin/sh\n").unwrap();
        let out = dir.join("pkg.onelf");
        let mut opts = base_opts(&app, &out, "bin/run");

        opts.update_url = Some("http://example.com/app.onelf.zsync".to_string());
        let err = pack(&opts, b"stub-runtime").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!out.exists());

        opts.update_url = Some("https://example.com/app.onelf.zsync".to_string());
        pack(&opts, b"stub-runtime").unwrap();

        let _ = fs::remove_dir_all(&dir);
    }

    fn sized(path: &str, size: u64) -> CollectedFile {
        CollectedFile {
            rel_path: PathBuf::from(path),
            source: FileSource::Memory(Vec::new()),
            size,
            mode: 0o644,
            mtime_secs: 0,
            mtime_nsec: 0,
        }
    }

    #[test]
    fn a_profile_reads_as_paths_and_offsets() {
        let text = "# a comment\nlib/libfoo.so\t0\n\nlib/libfoo.so\t65536\nbin/app\t4096\n";
        assert_eq!(
            parse_profile(text).unwrap(),
            vec![
                ("lib/libfoo.so".to_string(), 0),
                ("lib/libfoo.so".to_string(), 65536),
                ("bin/app".to_string(), 4096),
            ]
        );
        assert_eq!(parse_profile("lib/libfoo.so\n").unwrap_err().0, 1);
        assert_eq!(parse_profile("bin/app\t0\nlib/x\tten\n").unwrap_err().0, 2);
    }

    /// The prefetch list keeps the profile's order, lands each offset on
    /// the block holding it at this block size, names each block once, and
    /// leaves out what this tree does not have.
    #[test]
    fn the_prefetch_list_follows_the_profile_through_this_tree() {
        let files = vec![sized("bin/app", 100_000), sized("lib/libfoo.so", 300_000)];
        let profile: Vec<(String, u64)> = [
            ("lib/libfoo.so", 200_000),
            ("bin/app", 0),
            ("./lib/libfoo.so", 0),
            ("lib/libfoo.so", 70_000),
            ("lib/libgone.so", 0),
            ("bin/app", 100_000),
            ("lib/libfoo.so", 1),
        ]
        .iter()
        .map(|(p, o)| (p.to_string(), *o))
        .collect();
        assert_eq!(
            prefetch_lines(&files, &profile, 64 * 1024),
            [
                "lib/libfoo.so\t3",
                "bin/app\t0",
                "lib/libfoo.so\t0",
                "lib/libfoo.so\t1"
            ]
        );
        assert_eq!(
            prefetch_lines(&files, &profile, 256 * 1024),
            ["lib/libfoo.so\t0", "bin/app\t0"]
        );
    }
}
