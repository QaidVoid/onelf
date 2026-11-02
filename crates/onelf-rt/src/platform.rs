//! The GL build a package pins, for a host that has no GL stack of its
//! own.
//!
//! The package records the build's label, URL and BLAKE3 hash in
//! `.onelf/platform`. The build is fetched once per label into the
//! shared store under the cache root, and every acquisition verifies the
//! stored bytes against that hash through a single held file handle
//! before anything extracts or serves them, so two packages pinning the
//! same build share both the download and the verification. Extraction
//! goes through the ordinary package cache. Every failure is reported by
//! the caller as a warning and the launch goes on without a GL stack.
//!
//! Where the store lives is decided by the target uid alone: an ordinary
//! launch keeps its explicit relocation, a privileged launch keeps only
//! its own private per-uid base, and the inherited `ONELF_PLATFORM_STORE`
//! hint never overrides that ownership decision.
//!
//! A stored candidate is pinned before anything reads it: the
//! store-relative name is walked one component at a time with
//! `O_PATH|O_NOFOLLOW`, must be a regular file by the `fstat` of its
//! pinned descriptor, and is converted to a readable handle in place, so
//! a symlink planted inside the store, a name swapped while the candidate
//! is held, or a FIFO, device, socket or directory at the stored name
//! never activates content I/O. The store root itself is reached by name
//! and may be a symlink: it is the location the target uid owns or chose.

use std::fs;
use std::io::{self, Read};
use std::ops::Deref;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags, fstat};
use rustix::io::Errno;

/// Where the package records its pin, relative to the package root.
pub const PIN_FILE: &str = ".onelf/platform";

/// A GL build pinned by hash.
#[derive(Debug, PartialEq, Eq)]
pub struct Pin {
    pub label: String,
    pub url: String,
    pub blake3: String,
}

/// Parse the record's `key = "value"` lines. Values are TOML basic
/// strings as the packer writes them, with `\"` and `\\` escapes.
pub fn parse_pin(text: &str) -> Option<Pin> {
    let (mut label, mut url, mut blake3) = (None, None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = value.strip_prefix('"')?.strip_suffix('"')?;
        let value = value.replace("\\\"", "\"").replace("\\\\", "\\");
        match key.trim() {
            "label" => label = Some(value),
            "url" => url = Some(value),
            "blake3" => blake3 = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    checked_pin(label?, url?, blake3?)
}

/// A pin, if the three values can name a stored build: the label is a
/// directory name and the hash names the file and is compared against
/// its bytes, so it has to be exactly one BLAKE3 digest in hex.
pub fn checked_pin(label: String, url: String, blake3: String) -> Option<Pin> {
    if label.is_empty() || label.contains('/') || label.starts_with('.') {
        return None;
    }
    if blake3.len() != 64 || !blake3.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(Pin { label, url, blake3 })
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Make the build the package at `pkg_root` pins available: fetched if
/// the store lacks it, verified, extracted. Returns the extracted root
/// and the lock that keeps the extraction from being collected while
/// this instance runs, or `None` when the package pins no build. The
/// error names why the pinned build could not be used.
pub fn obtain(pkg_root: &Path) -> Option<Result<(PathBuf, fs::File), String>> {
    let text = fs::read_to_string(pkg_root.join(PIN_FILE)).ok()?;
    let Some(mut pin) = parse_pin(&text) else {
        return Some(Err(format!("{PIN_FILE} names no usable build")));
    };
    if let Some(url) = std::env::var("ONELF_PLATFORM_URL")
        .ok()
        .filter(|u| !u.is_empty())
    {
        pin.url = url;
    }

    Some(obtain_build(&pin))
}

/// Make the build `pin` names available: fetched into the shared store if
/// it is not there, verified against the hash before it is used, then
/// extracted through the ordinary package cache. Returns the extracted
/// root and the lock that keeps it from being collected while this
/// instance runs.
///
/// Shared by the GL build and by every dependency set, which differ only
/// in when they are wanted, not in how they are obtained.
pub fn obtain_build(pin: &Pin) -> Result<(PathBuf, fs::File), String> {
    let artifact = store_build(pin)?;
    extract_build(&artifact)
}

/// A cached build opened once and verified against its pin.
///
/// The handle owns the open file description the digest was computed
/// over, so every consumer after [`store_build`] reads the bytes that
/// were verified: extraction loads through a duplicate of the handle,
/// and a file swapped in at the stored name afterwards is never seen.
/// The `Deref` to the stored path exists for consumers that must be
/// handed a path; the set mount server's re-open of that name is the one
/// lookup this leaves, closed later by descriptor-based traversal.
#[derive(Debug)]
pub struct VerifiedArtifact {
    path: PathBuf,
    file: fs::File,
}

impl Deref for VerifiedArtifact {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

/// Extract the build `artifact` holds through the ordinary package cache.
/// The container is loaded from a duplicate of the verified handle, so
/// the bytes are the ones whose digest matched the pin.
pub fn extract_build(artifact: &VerifiedArtifact) -> Result<(PathBuf, fs::File), String> {
    let file = artifact
        .file
        .try_clone()
        .map_err(|e| format!("{}: {e}", artifact.path.display()))?;
    let mut pkg = crate::loader::load_file(file)
        .map_err(|e| format!("{}: not a usable build: {e}", artifact.path.display()))?;
    crate::cache::ensure_extracted(&mut pkg)
        .map_err(|e| format!("{}: cannot extract: {e}", artifact.path.display()))
}

/// The inherited import hint: where the launch that started this one
/// resolved its ordinary store, exported so the same pinned build is not
/// downloaded twice. A suggestion only. It pre-fills the store candidates
/// an ordinary target accepts, and it never overrides the ownership
/// decision: the name arrived through the environment of the invoking
/// user, so a privileged target refuses it as a destination, and no
/// guarantee depends on a privilege transition keeping it.
fn import_hint() -> Option<PathBuf> {
    std::env::var_os("ONELF_PLATFORM_STORE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The store directory the target uid owns, from the uid's decision over
/// the inherited hint and the candidate bases.
///
/// An ordinary uid keeps its relocation: the hint names the store this
/// user chose, and without one the per-user cache serves. A privileged
/// uid takes only its own private base. Every environment-derived name
/// root could otherwise write through, the hint as well as the
/// HOME/XDG-derived cache root, was chosen by the unprivileged caller,
/// so both are refused, and neither candidate base is read unless the
/// decision needs it.
///
/// The bases are closures so a refusal costs no probe: resolving root's
/// store must not create or stat anything under the paths the
/// environment names.
fn owned_store(
    uid: u32,
    hint: Option<&Path>,
    cache_base: impl FnOnce() -> Option<PathBuf>,
    private_base: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    let hint = hint
        .filter(|v| !v.as_os_str().is_empty())
        .map(PathBuf::from);
    if uid != 0 {
        if let Some(dir) = hint {
            return Ok(dir);
        }
        return cache_base()
            .map(|base| base.join("platform"))
            .ok_or_else(|| {
                "no cache directory to store a build in (set HOME or XDG_CACHE_HOME)".to_string()
            });
    }
    private_base()
        .map(|base| base.join("platform"))
        .ok_or_else(|| "no root-owned private directory to store a build in".to_string())
}

/// Where the shared store keeps the build `pin` names, whether or not it
/// is there yet. The store is the one the target uid owns; see
/// [`owned_store`] for the ownership decision.
pub fn stored_path(pin: &Pin) -> Result<PathBuf, String> {
    let (store, relative) = stored_location(pin)?;
    Ok(store.join(relative))
}

/// [`stored_path`] split into the store root and the store-relative
/// candidate the confined walk needs: the label directory and the
/// hash-named file. The name proves nothing about the bytes: every
/// acquisition verifies them against the pin before use.
fn stored_location(pin: &Pin) -> Result<(PathBuf, String), String> {
    let hint = import_hint();
    let store = owned_store(
        crate::paths::uid(),
        hint.as_deref(),
        crate::cache::base_dir,
        crate::paths::private_dir,
    )?;
    // The build is stored by its own hash, so two builds a label pins over
    // its lifetime coexist rather than overwrite each other and force a
    // refetch on every launch.
    Ok((store, format!("{}/{}.onelf", pin.label, pin.blake3)))
}

/// The build `pin` names, from the shared store: the cached file is
/// pinned and hashed through confined descriptors, and only a missing or
/// mismatching file is fetched, verified, and published in its place.
/// Returns the verified handle every consumer reads through.
pub fn store_build(pin: &Pin) -> Result<VerifiedArtifact, String> {
    let (store, relative) = stored_location(pin)?;
    acquire(&store, &relative, pin, !env_set("ONELF_NO_PLATFORM_FETCH"))
}

/// [`store_build`] against an explicit store root, the store-relative
/// candidate name, and a fetch policy, so the acquisition the GL build and
/// the dependency sets share is testable without touching either's
/// environment.
fn acquire(
    store: &Path,
    relative: &str,
    pin: &Pin,
    allow_fetch: bool,
) -> Result<VerifiedArtifact, String> {
    let path = store.join(relative);
    let origin = path.display().to_string();
    let mut refused: Option<String> = None;
    match open_verified(store, relative, &pin.blake3, &origin) {
        Ok(file) => {
            let artifact = VerifiedArtifact { path, file };
            touch(&artifact.file);
            return Ok(artifact);
        }
        Err(PinError::Missing) => {}
        Err(PinError::Final(why)) => refused = Some(why),
        // A component above the candidate cannot be traversed safely, or
        // the pinned inode has no readable conversion here: neither is
        // repaired by fetching, and neither may fall back to opening the
        // untrusted path by name.
        Err(PinError::Intermediate(why) | PinError::Conversion(why)) => return Err(why),
    }
    // The stored copy is missing or does not match its pin, so it is a
    // cache miss. What is fetched next repairs the store; when no
    // replacement is permitted, the refusal is itself the failure.
    let miss = |why: String| match &refused {
        Some(refused) => format!("{refused}; {why}"),
        None => why,
    };
    if !allow_fetch {
        return Err(miss(format!(
            "fetching is disabled by ONELF_NO_PLATFORM_FETCH ({} would be fetched from {})",
            pin.label, pin.url
        )));
    }
    let Some((label, _)) = relative.rsplit_once('/') else {
        return Err(miss("the candidate names no label directory".to_string()));
    };
    // The walk refused any label component that is not a real directory,
    // so this creation and the rename below cannot traverse out of the
    // store.
    let label_dir = store.join(label);
    fs::create_dir_all(&label_dir).map_err(|e| miss(format!("{}: {e}", label_dir.display())))?;
    let tmp_relative = format!("{label}/.{}.onelf.{}", pin.blake3, std::process::id());
    let tmp = store.join(&tmp_relative);
    let file = match fetch(&pin.url, &tmp).and_then(|()| {
        open_verified(store, &tmp_relative, &pin.blake3, &pin.url).map_err(PinError::message)
    }) {
        Ok(file) => file,
        Err(why) => {
            let _ = fs::remove_file(&tmp);
            return Err(miss(why));
        }
    };
    // Publishing renames over whatever the stored name held: rename never
    // opens the entry it replaces, so a planted symlink or special object
    // at the name is swapped out without being touched.
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        miss(format!("{}: {e}", path.display()))
    })?;
    touch(&file);
    Ok(VerifiedArtifact { path, file })
}

/// Why the confined open of a store candidate did not produce a verified
/// readable handle. Nothing was read from the candidate in any case.
#[derive(Debug)]
enum PinError {
    /// The candidate is not there: an ordinary cache miss a fetch may
    /// repair.
    Missing,
    /// The final name holds something a store build cannot be: a symlink
    /// or a non-regular object. No content was read; a fetched copy
    /// renamed over the name repairs it without opening it.
    Final(String),
    /// A component above the final name cannot be traversed safely, so
    /// nothing under it may be read or written through it.
    Intermediate(String),
    /// The pinned regular file has no readable handle for the same inode,
    /// so no handle could serve the pinned bytes.
    Conversion(String),
}

impl PinError {
    /// The bounded refusal for a caller that cannot retry the lookup.
    fn message(self) -> String {
        match self {
            PinError::Missing => "the candidate went missing while it was held".to_string(),
            PinError::Final(why) | PinError::Intermediate(why) | PinError::Conversion(why) => why,
        }
    }
}

/// A store candidate pinned by descriptor: a regular file reached through
/// a walk that opened nothing for content and followed nothing inside the
/// store, held so that conversion and hashing cannot be separated from
/// the pinned inode by a rename.
struct PinnedFile {
    fd: OwnedFd,
}

impl PinnedFile {
    /// Pin the store-relative candidate `relative` under `store`.
    ///
    /// The store root is opened by name with symlink following kept: it is
    /// the location the target uid owns or chose. Every component inside
    /// it is opened with `openat(O_PATH|O_NOFOLLOW)`, so a symlink, FIFO,
    /// device, socket or ordinary file in place of a directory stops the
    /// walk before anything under it is touched, and a symlink or
    /// non-regular object at the final name is refused by the `fstat`
    /// file-type check without being opened for I/O.
    fn pin(store: &Path, relative: &str, origin: &str) -> Result<PinnedFile, PinError> {
        let mut dir = match rustix::fs::open(
            store,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(dir) => dir,
            Err(Errno::NOENT) => return Err(PinError::Missing),
            Err(e) => return Err(PinError::Intermediate(format!("{origin}: {e}"))),
        };
        let names: Vec<&str> = relative.split('/').collect();
        if names.len() > 8 {
            return Err(PinError::Intermediate(format!(
                "{origin}: too many path components"
            )));
        }
        let last = names.len() - 1;
        for (index, name) in names.iter().enumerate() {
            if name.is_empty() || matches!(*name, "." | "..") {
                return Err(PinError::Intermediate(format!(
                    "{origin}: unsafe path component {name}"
                )));
            }
            let pinned = match rustix::fs::openat(
                &dir,
                *name,
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(pinned) => pinned,
                Err(Errno::NOENT) => return Err(PinError::Missing),
                Err(e) => {
                    return Err(PinError::Intermediate(format!("{origin}: {name}: {e}")));
                }
            };
            let kind = kind_name(FileType::from_raw_mode(
                fstat(&pinned)
                    .map_err(|e| PinError::Intermediate(format!("{origin}: {name}: {e}")))?
                    .st_mode,
            ));
            if index == last {
                if kind != KIND_REGULAR {
                    return Err(PinError::Final(format!(
                        "{origin}: {kind} at the stored name, not a regular file"
                    )));
                }
                return Ok(PinnedFile { fd: pinned });
            }
            if kind != KIND_DIRECTORY {
                return Err(PinError::Intermediate(format!(
                    "{origin}: {name} is {kind}, not a directory"
                )));
            }
            dir = pinned;
        }
        Err(PinError::Intermediate(format!("{origin}: empty candidate")))
    }

    /// Convert the pinned descriptor into a readable [`fs::File`] for the
    /// same inode, without any lookup through a path.
    ///
    /// Linux reopens the descriptor in place: `openat` with an empty path
    /// and `O_RDONLY|O_NOFOLLOW|O_EMPTY_PATH` operates on `dirfd` itself.
    /// The reopened descriptor is accepted only when `fstat` reports the
    /// pinned device and inode, so nothing but the pinned regular file can
    /// be served. Where the mechanism is missing, or the pinned file
    /// cannot be reopened, the conversion is refused; the untrusted path
    /// is never reopened by name.
    fn readable(self, origin: &str) -> Result<fs::File, PinError> {
        let PinnedFile { fd } = self;
        let pinned = fstat(&fd).map_err(|e| PinError::Conversion(format!("{origin}: {e}")))?;
        let reopened = reopen_in_place(&fd).map_err(|e| {
            PinError::Conversion(format!(
                "{origin}: the pinned file cannot be reopened in place: {e}"
            ))
        })?;
        let got = fstat(&reopened).map_err(|e| PinError::Conversion(format!("{origin}: {e}")))?;
        if (got.st_dev, got.st_ino) != (pinned.st_dev, pinned.st_ino) {
            return Err(PinError::Conversion(format!(
                "{origin}: the reopened handle does not match the pinned inode"
            )));
        }
        Ok(fs::File::from(reopened))
    }
}

/// The [`kind_name`] of the only file type a store candidate may be.
const KIND_REGULAR: &str = "a regular file";

/// The [`kind_name`] of the only file type a walk may descend into.
const KIND_DIRECTORY: &str = "a directory";

/// The bounded name of a file type, for refusals that say what was found
/// instead of what was wanted.
fn kind_name(kind: FileType) -> &'static str {
    match kind {
        FileType::RegularFile => KIND_REGULAR,
        FileType::Directory => KIND_DIRECTORY,
        FileType::Symlink => "a symlink",
        FileType::Fifo => "a FIFO",
        FileType::Socket => "a socket",
        FileType::CharacterDevice => "a character device",
        FileType::BlockDevice => "a block device",
        FileType::Unknown => "an unknown object",
    }
}

/// The Linux `O_EMPTY_PATH` open bit, which makes `openat` with an empty
/// path operate on `dirfd` itself. Linux-specific and absent from
/// `rustix`'s flag set, so it is spelled by value.
#[cfg(target_os = "linux")]
const EMPTY_PATH: OFlags = OFlags::from_bits_retain(0x0400_0000);

/// Reopen `pinned` for reading without a pathname lookup: Linux accepts
/// an empty path in `openat` and reopens the descriptor itself. On
/// kernels and sandboxes without the bit the open fails, and the caller
/// refuses the conversion.
#[cfg(target_os = "linux")]
fn reopen_in_place(pinned: &OwnedFd) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(
        pinned,
        "",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | EMPTY_PATH,
        Mode::empty(),
    )?)
}

/// A platform without the Linux reopen mechanism has no safe conversion,
/// so every conversion is refused.
#[cfg(not(target_os = "linux"))]
fn reopen_in_place(_pinned: &OwnedFd) -> io::Result<OwnedFd> {
    Err(io::Error::other(
        "no same-inode reopen mechanism on this platform",
    ))
}

/// Open the store candidate `relative` under `store` through the confined
/// walk, convert its pinned inode to a readable handle, and hash the
/// handle's bytes against `expected`. Returns that handle, so every
/// consumer reads the bytes whose digest matched.
fn open_verified(
    store: &Path,
    relative: &str,
    expected: &str,
    origin: &str,
) -> Result<fs::File, PinError> {
    let pinned = PinnedFile::pin(store, relative, origin)?;
    let file = pinned.readable(origin)?;
    let got = hash_stream(&file).map_err(|e| PinError::Final(format!("{origin}: {e}")))?;
    if got != expected {
        return Err(PinError::Final(format!(
            "{origin}: hash mismatch (got {got}, pinned {expected})"
        )));
    }
    Ok(file)
}

/// The BLAKE3 digest of everything readable from `file`, in lowercase
/// hex. Fixed-size reads keep memory bounded whatever the file's size.
fn hash_stream(file: &fs::File) -> io::Result<String> {
    let mut file = file;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Copy or download the build to `into`. The scheme decides: a local
/// file is copied, `https://` goes through the updater's client, and
/// anything else is refused before a request is made.
fn fetch(url: &str, into: &Path) -> Result<(), String> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::copy(path, into)
            .map(|_| ())
            .map_err(|e| format!("{url}: {e}"));
    }
    if url.starts_with("https://") {
        return download(url, into);
    }
    Err(format!("{url}: a build must be an https:// or file:// URL"))
}

#[cfg(feature = "update")]
fn download(url: &str, into: &Path) -> Result<(), String> {
    crate::update::download(url, into)
}

#[cfg(not(feature = "update"))]
fn download(url: &str, _into: &Path) -> Result<(), String> {
    Err(format!("this runtime has no HTTPS client to fetch {url}"))
}

/// Record a use through the verified handle, so collection can tell an
/// idle build from one in use.
fn touch(file: &fs::File) {
    let _ = file.set_modified(std::time::SystemTime::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    use onelf_format::{
        Block, Entry, EntryKind, EntryPoint, Flags, Footer, Manifest, ManifestHeader,
    };

    /// The one file a test container carries, and its stored bytes.
    const FILE_NAME: &str = "lib/libdemo.so";
    const FILE_CONTENT: &[u8] = b"demo set library bytes\n";

    /// A minimal stored-mode container the runtime loader can open,
    /// together with the digest that names it in a store.
    struct Container {
        bytes: Vec<u8>,
        digest: String,
    }

    /// Build one: `[prefix][manifest][stored payload][footer]`, one
    /// verbatim block for [`FILE_CONTENT`].
    fn container() -> Container {
        let prefix = b"onelf platform fixture";
        let mut string_table: Vec<u8> = vec![0];
        let name_offset = string_table.len() as u32;
        string_table.extend_from_slice(FILE_NAME.as_bytes());
        string_table.push(0);

        let hash = *blake3::hash(FILE_CONTENT).as_bytes();
        let entry = Entry {
            kind: EntryKind::File,
            parent: u32::MAX,
            name: name_offset,
            mode: 0o755,
            mtime_secs: 0,
            mtime_nsec: 0,
            content_hash: hash,
            blocks: vec![Block {
                payload_offset: 0,
                compressed_size: FILE_CONTENT.len() as u64,
                original_size: FILE_CONTENT.len() as u64,
                content_hash: hash,
            }],
            symlink_target: 0,
        };
        let manifest = Manifest {
            header: ManifestHeader {
                version: 3,
                entry_count: 1,
                string_table_size: string_table.len() as u32,
                entrypoint_count: 0,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [0; 32],
                prefix_hash: *blake3::hash(prefix).as_bytes(),
                footer_hash: [0; 32],
            },
            entrypoints: Vec::<EntryPoint>::new(),
            entries: vec![entry],
            lib_dir_offsets: Vec::new(),
            string_table,
        };
        let raw = manifest.serialize().unwrap();
        let compressed = zstd::bulk::compress(&raw, 1).unwrap();
        let manifest_offset = prefix.len() as u64;
        let payload_offset = manifest_offset + compressed.len() as u64;
        let footer = Footer {
            format_version: onelf_format::footer::FORMAT_VERSION,
            flags: Flags::STORED,
            manifest_offset,
            manifest_compressed: compressed.len() as u64,
            manifest_original: raw.len() as u64,
            payload_offset,
            payload_size: FILE_CONTENT.len() as u64,
            dict_offset: 0,
            dict_size: 0,
            manifest_checksum: onelf_format::manifest::manifest_checksum(&raw).unwrap(),
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(FILE_CONTENT);
        footer.write_to(&mut bytes).unwrap();
        let digest = blake3::hash(&bytes).to_hex().to_string();
        Container { bytes, digest }
    }

    /// A private store directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("onelf-platform-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Where the store keeps a build with `digest` inside `dir`.
    fn stored_in(dir: &Path, digest: &str) -> PathBuf {
        dir.join("set").join(format!("{digest}.onelf"))
    }

    /// The store-relative candidate name of a build with `digest`, under
    /// the label every test uses.
    fn stored_rel(digest: &str) -> String {
        format!("set/{digest}.onelf")
    }

    /// The pin whose cached build lives at `digest`, with `source` as the
    /// configured fetch source.
    fn pin_for(digest: &str, source: &Path) -> Pin {
        Pin {
            label: "set".into(),
            url: format!("file://{}", source.display()),
            blake3: digest.into(),
        }
    }

    /// Load the container again through a duplicate of the held handle.
    fn reload(artifact: &VerifiedArtifact) -> crate::loader::PackageData {
        crate::loader::load_file(artifact.file.try_clone().unwrap()).unwrap()
    }

    /// The verified payload of [`FILE_NAME`] as the loader reads it from
    /// `pkg`.
    fn entry_bytes(pkg: &crate::loader::PackageData) -> Vec<u8> {
        let idx = (0..pkg.manifest.entries.len())
            .find(|&i| pkg.manifest.entry_path(i) == FILE_NAME)
            .expect("the container carries the file");
        crate::loader::read_verified_entry(
            &pkg.file,
            &pkg.footer,
            &pkg.manifest.entries[idx],
            &pkg.codec,
        )
        .unwrap()
    }

    #[test]
    fn a_record_parses_and_unescapes() {
        let hash = "AB".repeat(32);
        let pin = parse_pin(&format!(
            "label = \"platform-1\"\nurl = \"https://e/a\\\"b\"\nblake3 = \"{hash}\"\n"
        ))
        .unwrap();
        assert_eq!(pin.label, "platform-1");
        assert_eq!(pin.url, "https://e/a\"b");
        assert_eq!(pin.blake3, "ab".repeat(32));
        assert!(parse_pin("label = \"x\"\n").is_none(), "incomplete");
        assert!(
            parse_pin(&format!(
                "label = \"../x\"\nurl = \"u\"\nblake3 = \"{hash}\"\n"
            ))
            .is_none(),
            "a label is a directory name"
        );
        assert!(
            parse_pin("label = \"x\"\nurl = \"u\"\nblake3 = \"abc\"\n").is_none(),
            "the hash names a file and is checked against bytes"
        );
    }

    #[test]
    fn a_mismatch_or_bad_scheme_leaves_nothing_behind() {
        let dir = temp_dir("mismatch");
        let source = dir.join("source");
        fs::write(&source, b"not the pinned bytes").unwrap();
        let pin = Pin {
            label: "l".into(),
            url: format!("file://{}", source.display()),
            blake3: "0".repeat(64),
        };
        let tmp_relative = format!("l/.{}.onelf.{}", pin.blake3, std::process::id());
        let tmp = dir.join(&tmp_relative);
        fs::create_dir_all(tmp.parent().unwrap()).unwrap();
        let err = fetch(&pin.url, &tmp)
            .and_then(|()| {
                open_verified(&dir, &tmp_relative, &pin.blake3, &pin.url).map_err(PinError::message)
            })
            .unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        let err = fetch("http://e/gl.onelf", &tmp).unwrap_err();
        assert!(err.contains("https:// or file://"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bitflipped_cached_file_is_rejected_before_any_load() {
        let dir = temp_dir("bitflip");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        let mut bytes = c.bytes.clone();
        bytes[5] ^= 0xff;
        fs::write(&stored, &bytes).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&c.digest), &pin, false).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(
            !err.contains("not a usable build"),
            "the loader must not run on unverified bytes: {err}"
        );
        assert_eq!(fs::read(&stored).unwrap(), bytes, "the store is left alone");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_cached_container_is_rejected_before_any_load() {
        let dir = temp_dir("truncated");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        let bytes = &c.bytes[..c.bytes.len() - 40];
        fs::write(&stored, bytes).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&c.digest), &pin, false).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(
            !err.contains("not a usable build") && !err.contains("too small"),
            "the loader must not run on unverified bytes: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_verified_file_flows_to_consumers_through_its_handle() {
        let dir = temp_dir("handle");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let artifact = acquire(&dir, &stored_rel(&c.digest), &pin, false).unwrap();
        assert_eq!(&*artifact, &*stored);
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);

        // The name is swapped for other bytes and then removed; the held
        // inode is what every consumer keeps reading.
        let swapped = dir.join("swapped");
        fs::write(&swapped, b"swapped in place").unwrap();
        fs::rename(&swapped, &stored).unwrap();
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        fs::remove_file(&stored).unwrap();
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_store_file_is_refetched_and_replaced() {
        let dir = temp_dir("refetch");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, b"damaged in place").unwrap();
        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();

        let pin = pin_for(&c.digest, &source);
        let artifact = acquire(&dir, &stored_rel(&c.digest), &pin, true).unwrap();
        assert_eq!(fs::read(&stored).unwrap(), c.bytes, "the store is repaired");
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        assert_eq!(
            fs::read_dir(stored.parent().unwrap()).unwrap().count(),
            1,
            "no fetch scratch is left behind"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_refetch_reports_the_integrity_failure() {
        let dir = temp_dir("refetch-fails");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, b"damaged in place").unwrap();
        let source = dir.join("source");
        fs::write(&source, b"not the pinned bytes either").unwrap();

        let pin = pin_for(&c.digest, &source);
        let err = acquire(&dir, &stored_rel(&c.digest), &pin, true).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert_eq!(
            fs::read_dir(stored.parent().unwrap()).unwrap().count(),
            1,
            "the rejected fetch leaves only the stored file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_store_file_with_fetch_disabled_reports_the_mismatch() {
        let dir = temp_dir("empty");
        let stored = stored_in(&dir, &"0".repeat(64));
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, b"").unwrap();

        let pin = pin_for(&"0".repeat(64), &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&"0".repeat(64)), &pin, false).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(err.contains("ONELF_NO_PLATFORM_FETCH"), "{err}");

        let missing = stored_in(&dir, &"1".repeat(64));
        assert!(!missing.exists(), "the second entry is absent");
        let err = acquire(
            &dir,
            &stored_rel(&"1".repeat(64)),
            &pin_for(&"1".repeat(64), &dir.join("absent")),
            false,
        )
        .unwrap_err();
        assert!(err.contains("fetching is disabled"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A symlink at the stored name is refused without reading through
    /// it, even when its target carries exactly the pinned bytes: the
    /// refusal is a property of the path, not of a digest. With fetching
    /// allowed, a fetched copy renamed over the link repairs the store.
    #[test]
    fn a_symlink_at_the_stored_name_is_refused_and_replaced() {
        let dir = temp_dir("symlink-final");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        let target = dir.join("elsewhere");
        fs::write(&target, &c.bytes).unwrap();
        std::os::unix::fs::symlink(&target, &stored).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&c.digest), &pin, false).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(err.contains("not a regular file"), "{err}");
        assert!(err.contains("ONELF_NO_PLATFORM_FETCH"), "{err}");
        assert_eq!(
            fs::read_link(&stored).unwrap(),
            target,
            "the refusal leaves the link alone"
        );

        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();
        let artifact = acquire(
            &dir,
            &stored_rel(&c.digest),
            &pin_for(&c.digest, &source),
            true,
        )
        .unwrap();
        assert!(!stored.is_symlink(), "the fetched copy is a real file");
        assert_eq!(fs::read(&stored).unwrap(), c.bytes);
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A symlinked label directory is refused outright: nothing under it
    /// is read even when the bytes behind the link match the pin, and no
    /// fetch is published through the link. The refusal stands with
    /// fetching allowed.
    #[test]
    fn a_symlinked_label_directory_is_refused_without_traversal() {
        let dir = temp_dir("symlink-label");
        let c = container();
        let evil = dir.join("evil");
        fs::create_dir_all(&evil).unwrap();
        fs::write(evil.join(format!("{}.onelf", c.digest)), &c.bytes).unwrap();
        std::os::unix::fs::symlink(&evil, dir.join("set")).unwrap();
        let source = dir.join("source");
        fs::write(&source, &c.bytes).unwrap();

        let pin = pin_for(&c.digest, &source);
        for allow_fetch in [false, true] {
            let err = acquire(&dir, &stored_rel(&c.digest), &pin, allow_fetch).unwrap_err();
            assert!(err.contains("symlink"), "{err}");
            assert_eq!(
                fs::read_dir(&evil).unwrap().count(),
                1,
                "nothing is read or written through the link"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Special objects planted at the stored name, and at the label
    /// directory, are refused by the file-type check on the pinned
    /// descriptor before any content open exists. A FIFO is the proof: an
    /// open for reading would have blocked forever or read nothing, and
    /// either way the error would speak of a read, not of the file type.
    /// Devices share the one file-type gate these fixtures exercise with
    /// a FIFO, a socket and a directory.
    #[test]
    fn special_objects_are_refused_before_any_content_io() {
        use rustix::fs::mkfifoat;

        for tag in ["fifo", "socket", "dir"] {
            let dir = temp_dir(&format!("special-{tag}"));
            // A shortened name keeps the planted path inside a unix
            // socket's address limit; the refusal happens long before the
            // digest is consulted.
            let digest = "0".repeat(24);
            let label = dir.join("set");
            fs::create_dir_all(&label).unwrap();
            let at = stored_in(&dir, &digest);
            match tag {
                "fifo" => {
                    let handle = fs::File::open(&label).unwrap();
                    mkfifoat(&handle, format!("{digest}.onelf"), Mode::RUSR | Mode::WUSR).unwrap();
                }
                "socket" => {
                    let _listener = std::os::unix::net::UnixListener::bind(&at).unwrap();
                }
                _ => {
                    fs::create_dir(&at).unwrap();
                }
            }
            let pin = pin_for(&digest, &dir.join("absent-source"));
            let err = acquire(&dir, &stored_rel(&digest), &pin, false).unwrap_err();
            assert!(err.contains("not a regular file"), "{tag}: {err}");
            assert!(
                !err.contains("hash mismatch"),
                "{tag}: no byte may have been read: {err}"
            );
            let _ = fs::remove_dir_all(&dir);
        }

        // A FIFO at the label directory stops the walk before the stored
        // name is even reached.
        let dir = temp_dir("special-fifo-label");
        let digest = "0".repeat(64);
        let root = fs::File::open(&dir).unwrap();
        mkfifoat(&root, "set", Mode::RUSR | Mode::WUSR).unwrap();
        drop(root);
        let pin = pin_for(&digest, &dir.join("absent-source"));
        let err = acquire(&dir, &stored_rel(&digest), &pin, false).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        assert!(
            !err.contains("hash mismatch"),
            "no byte may have been read: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The converted handle reads the pinned inode, not the stored name:
    /// bytes renamed in while the descriptor is pinned never reach the
    /// verification or the consumer.
    #[test]
    fn the_converted_handle_reads_the_pinned_inode_not_the_name() {
        let dir = temp_dir("conversion-swap");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();

        let pinned =
            PinnedFile::pin(&dir, &stored_rel(&c.digest), &stored.display().to_string()).unwrap();
        let hostile = dir.join("hostile");
        fs::write(&hostile, b"hostile replacement").unwrap();
        fs::rename(&hostile, &stored).unwrap();

        let file = pinned
            .readable(&stored.display().to_string())
            .map_err(PinError::message)
            .unwrap();
        let pkg = crate::loader::load_file(file).unwrap();
        assert_eq!(entry_bytes(&pkg), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A pinned regular file that cannot be reopened in place is refused
    /// with a bounded error, and nothing reopens the stored path by name:
    /// the readable hostile bytes renamed in at that name would otherwise
    /// have been hashed. Here the refusal comes from the file's own
    /// permissions; on a kernel without the Linux reopen mechanism the
    /// same refusal comes from the missing flag instead.
    #[test]
    fn an_unreadable_pinned_file_is_refused_without_reopening_by_name() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("conversion-refused");
        let c = container();
        let stored = stored_in(&dir, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();
        let mut perms = fs::metadata(&stored).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&stored, perms).unwrap();

        let pinned =
            PinnedFile::pin(&dir, &stored_rel(&c.digest), &stored.display().to_string()).unwrap();
        let hostile = dir.join("hostile");
        fs::write(&hostile, b"hostile replacement").unwrap();
        fs::rename(&hostile, &stored).unwrap();

        match pinned.readable(&stored.display().to_string()) {
            Ok(_) => assert_eq!(
                crate::paths::uid(),
                0,
                "only a uid that may read the file converts it"
            ),
            Err(PinError::Conversion(why)) => {
                assert!(
                    why.contains("reopened in place") || why.contains("does not match"),
                    "{why}"
                );
            }
            Err(why) => panic!(
                "the refusal has the wrong class: {}",
                PinError::message(why)
            ),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The store root is reached by name on purpose: it is the location
    /// the target uid owns or chose, and a symlink there resolves as the
    /// user arranged it. Everything inside it is confined.
    #[test]
    fn a_symlinked_store_root_is_followed_but_nothing_inside_it() {
        let dir = temp_dir("symlink-root");
        let c = container();
        let real = dir.join("real");
        let stored = stored_in(&real, &c.digest);
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, &c.bytes).unwrap();
        let link = dir.join("store");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let pin = pin_for(&c.digest, &dir.join("absent-source"));
        let artifact = acquire(&link, &stored_rel(&c.digest), &pin, false).unwrap();
        assert_eq!(&*artifact, link.join(stored_rel(&c.digest)));
        assert_eq!(entry_bytes(&reload(&artifact)), FILE_CONTENT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An ordinary uid's relocation resolves exactly as given, and no
    /// candidate base is even consulted for it.
    #[test]
    fn an_ordinary_uid_keeps_the_relocated_store() {
        let dir = temp_dir("relocate");
        let store = owned_store(
            1000,
            Some(dir.as_path()),
            || unreachable!("the cache base is not consulted"),
            || unreachable!("the private base is not consulted"),
        )
        .unwrap();
        assert_eq!(store, dir);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Without a hint, and with an empty one, the per-user cache serves.
    #[test]
    fn an_ordinary_uid_without_a_hint_uses_the_cache_base() {
        let base = temp_dir("cachebase");
        for hint in [None, Some(Path::new(""))] {
            let store = owned_store(
                1000,
                hint,
                || Some(base.clone()),
                || unreachable!("the private base is not consulted"),
            )
            .unwrap();
            assert_eq!(store, base.join("platform"));
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn an_ordinary_uid_without_a_cache_base_is_an_error() {
        let err = owned_store(1000, None, || None, || unreachable!()).unwrap_err();
        assert!(err.contains("no cache directory"), "{err}");
    }

    /// Root writes nowhere the environment names. The inherited hint and
    /// the HOME/XDG-derived cache root were both chosen by the
    /// unprivileged caller; only the validated root-owned private base
    /// serves, and without one the decision fails closed. The refusals
    /// hold for a hint naming a user-owned store and for no hint at all.
    #[test]
    fn root_refuses_every_env_derived_store() {
        let hinted = Path::new("/home/attacker/.cache/onelf/platform");
        let store = owned_store(
            0,
            Some(hinted),
            || Some(PathBuf::from("/home/attacker/.cache/onelf")),
            || Some(PathBuf::from("/tmp/onelf-0")),
        )
        .unwrap();
        assert_eq!(store, PathBuf::from("/tmp/onelf-0/platform"));

        let store = owned_store(
            0,
            None,
            || Some(PathBuf::from("/home/attacker/.cache/onelf")),
            || Some(PathBuf::from("/tmp/onelf-0")),
        )
        .unwrap();
        assert_eq!(store, PathBuf::from("/tmp/onelf-0/platform"));

        let err = owned_store(0, Some(hinted), || None, || None).unwrap_err();
        assert!(err.contains("no root-owned private directory"), "{err}");
    }

    /// Refusing the cache root means never reading it: no stat, no
    /// creation, nothing under a path the environment chose.
    #[test]
    fn root_never_consults_the_cache_base() {
        let store = owned_store(
            0,
            None,
            || panic!("the HOME/XDG-derived cache root was consulted"),
            || Some(PathBuf::from("/tmp/onelf-0")),
        )
        .unwrap();
        assert_eq!(store, PathBuf::from("/tmp/onelf-0/platform"));
    }
}
