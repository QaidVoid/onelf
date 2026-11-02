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

use std::fs;
use std::io::{self, Read};
use std::ops::Deref;
use std::path::{Path, PathBuf};

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

/// Where the shared store keeps the build `pin` names, whether or not it
/// is there yet.
pub fn stored_path(pin: &Pin) -> Result<PathBuf, String> {
    let store = match std::env::var_os("ONELF_PLATFORM_STORE").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => crate::cache::base_dir()
            .ok_or("no cache directory to store a build in (set HOME or XDG_CACHE_HOME)")?
            .join("platform"),
    };
    // The build is stored by its own hash, so two builds a label pins over
    // its lifetime coexist rather than overwrite each other and force a
    // refetch on every launch. The name proves nothing about the bytes:
    // every acquisition verifies them against the pin before use.
    Ok(store.join(&pin.label).join(format!("{}.onelf", pin.blake3)))
}

/// The build `pin` names, from the shared store: the cached file is
/// opened once and hashed through that handle, and only a missing or
/// mismatching file is fetched, verified, and published in its place.
/// Returns the verified handle every consumer reads through.
pub fn store_build(pin: &Pin) -> Result<VerifiedArtifact, String> {
    acquire(&stored_path(pin)?, pin, !env_set("ONELF_NO_PLATFORM_FETCH"))
}

/// [`store_build`] against an explicit store path and fetch policy, so
/// the acquisition the GL build and the dependency sets share is
/// testable without touching either's environment.
fn acquire(path: &Path, pin: &Pin, allow_fetch: bool) -> Result<VerifiedArtifact, String> {
    let mut mismatch: Option<String> = None;
    // A plain stat first, so a named pipe or a directory at the stored
    // name cannot make the open below block or misbehave; the check that
    // counts happens on the opened handle.
    if fs::metadata(path).is_ok_and(|m| m.is_file()) {
        match verify_open(path, &pin.blake3, &path.display().to_string()) {
            Ok(file) => {
                let artifact = VerifiedArtifact {
                    path: path.to_path_buf(),
                    file,
                };
                touch(&artifact.file);
                return Ok(artifact);
            }
            Err(why) => mismatch = Some(why),
        }
    }
    // The stored copy is missing or does not match its pin, so it is a
    // cache miss. What is fetched next repairs the store; when no
    // replacement is permitted, the mismatch is itself the failure.
    let miss = |why: String| match &mismatch {
        Some(mismatch) => format!("{mismatch}; {why}"),
        None => why,
    };
    if !allow_fetch {
        return Err(miss(format!(
            "fetching is disabled by ONELF_NO_PLATFORM_FETCH ({} would be fetched from {})",
            pin.label, pin.url
        )));
    }
    let dir = path.parent().unwrap_or(path).to_path_buf();
    fs::create_dir_all(&dir).map_err(|e| miss(format!("{}: {e}", dir.display())))?;
    let tmp = dir.join(format!(".{}.onelf.{}", pin.blake3, std::process::id()));
    let file = match fetch(&pin.url, &tmp).and_then(|()| verify_open(&tmp, &pin.blake3, &pin.url)) {
        Ok(file) => file,
        Err(why) => {
            let _ = fs::remove_file(&tmp);
            return Err(miss(why));
        }
    };
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        miss(format!("{}: {e}", path.display()))
    })?;
    touch(&file);
    Ok(VerifiedArtifact {
        path: path.to_path_buf(),
        file,
    })
}

/// Open `path` once, hash the whole file streaming through that handle,
/// and return the handle only if the digest is `expected`. `origin` names
/// the bytes in errors: the stored path for a cache read, the source URL
/// for a fetch. Anything but a regular file is refused without hashing.
fn verify_open(path: &Path, expected: &str, origin: &str) -> Result<fs::File, String> {
    let bad = |e: String| format!("{origin}: {e}");
    let file = fs::File::open(path).map_err(|e| bad(e.to_string()))?;
    let meta = file.metadata().map_err(|e| bad(e.to_string()))?;
    if !meta.is_file() {
        return Err(format!("{origin}: not a regular file"));
    }
    let got = hash_stream(&file).map_err(|e| bad(e.to_string()))?;
    if got != expected {
        return Err(format!(
            "{origin}: hash mismatch (got {got}, pinned {expected})"
        ));
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
        let tmp = dir.join("tmp");
        let err = fetch(&pin.url, &tmp)
            .and_then(|()| verify_open(&tmp, &pin.blake3, &pin.url))
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
        let err = acquire(&stored, &pin, false).unwrap_err();
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
        let err = acquire(&stored, &pin, false).unwrap_err();
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
        let artifact = acquire(&stored, &pin, false).unwrap();
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
        let artifact = acquire(&stored, &pin, true).unwrap();
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
        let err = acquire(&stored, &pin, true).unwrap_err();
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
        let err = acquire(&stored, &pin, false).unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        assert!(err.contains("ONELF_NO_PLATFORM_FETCH"), "{err}");

        let missing = stored_in(&dir, &"1".repeat(64));
        let err = acquire(
            &missing,
            &pin_for(&"1".repeat(64), &dir.join("absent")),
            false,
        )
        .unwrap_err();
        assert!(err.contains("fetching is disabled"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
