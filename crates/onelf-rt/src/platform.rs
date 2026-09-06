//! The GL build a package pins, for a host that has no GL stack of its
//! own.
//!
//! The package records the build's label, URL and BLAKE3 hash in
//! `.onelf/platform`. The build is fetched once per label into the
//! shared store under the cache root, verified against the hash before
//! it is placed there, and extracted through the ordinary package cache
//! so two packages pinning the same build share both the download and
//! the extraction. Every failure is reported by the caller as a warning
//! and the launch goes on without a GL stack.

use std::fs;
use std::io::{self, Read};
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
/// this instance runs. The error names why no build could be used.
pub fn obtain(pkg_root: &Path) -> Result<(PathBuf, fs::File), String> {
    let text = fs::read_to_string(pkg_root.join(PIN_FILE)).ok();
    let mut pin = text
        .as_deref()
        .and_then(parse_pin)
        .ok_or("the package pins no GL build")?;
    if let Some(url) = std::env::var("ONELF_PLATFORM_URL")
        .ok()
        .filter(|u| !u.is_empty())
    {
        pin.url = url;
    }

    obtain_build(&pin)
}

/// Make the build `pin` names available: fetched into the shared store if
/// it is not there, verified against the hash before it is placed, then
/// extracted through the ordinary package cache. Returns the extracted
/// root and the lock that keeps it from being collected while this
/// instance runs.
///
/// Shared by the GL build and by every dependency set, which differ only
/// in when they are wanted, not in how they are obtained.
pub fn obtain_build(pin: &Pin) -> Result<(PathBuf, fs::File), String> {
    let file = store_build(pin)?;
    extract_build(&file)
}

/// Extract the build at `file` through the ordinary package cache.
pub fn extract_build(file: &Path) -> Result<(PathBuf, fs::File), String> {
    let mut pkg = crate::loader::load_from(file)
        .map_err(|e| format!("{}: not a usable build: {e}", file.display()))?;
    crate::cache::ensure_extracted(&mut pkg)
        .map_err(|e| format!("{}: cannot extract: {e}", file.display()))
}

/// The build `pin` names, in the shared store: fetched and verified if
/// the store lacks it. Returns the stored file.
pub fn store_build(pin: &Pin) -> Result<PathBuf, String> {
    let store = match std::env::var_os("ONELF_PLATFORM_STORE").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => crate::cache::base_dir()
            .ok_or("no cache directory to store a build in (set HOME or XDG_CACHE_HOME)")?
            .join("platform"),
    };
    // The build is stored by its own hash, so two builds a label pins over
    // its lifetime coexist rather than overwrite each other and force a
    // refetch on every launch. The name is the hash the bytes were
    // verified against, so an existing file needs no re-check.
    let dir = store.join(&pin.label);
    let file = dir.join(format!("{}.onelf", pin.blake3));

    if !file.is_file() {
        if env_set("ONELF_NO_PLATFORM_FETCH") {
            return Err(format!(
                "fetching is disabled by ONELF_NO_PLATFORM_FETCH ({} would be fetched from {})",
                pin.label, pin.url
            ));
        }
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = dir.join(format!(".{}.onelf.{}", pin.blake3, std::process::id()));
        let fetched = fetch(&pin.url, &tmp).and_then(|()| verify(&tmp, pin));
        if let Err(why) = fetched {
            let _ = fs::remove_file(&tmp);
            return Err(why);
        }
        fs::rename(&tmp, &file).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("{}: {e}", file.display())
        })?;
    }
    touch(&file);
    Ok(file)
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

fn verify(path: &Path, pin: &Pin) -> Result<(), String> {
    let got = hash_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if got != pin.blake3 {
        return Err(format!(
            "{}: hash mismatch (got {got}, pinned {})",
            pin.url, pin.blake3
        ));
    }
    Ok(())
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
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

/// Record a use, so collection can tell an idle build from one in use.
fn touch(file: &Path) {
    if let Ok(f) = fs::File::open(file) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = std::env::temp_dir().join(format!("onelf-platform-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source");
        fs::write(&source, b"not the pinned bytes").unwrap();
        let pin = Pin {
            label: "l".into(),
            url: format!("file://{}", source.display()),
            blake3: "0".repeat(64),
        };
        let tmp = dir.join("tmp");
        let err = fetch(&pin.url, &tmp)
            .and_then(|()| verify(&tmp, &pin))
            .unwrap_err();
        assert!(err.contains("hash mismatch"), "{err}");
        let err = fetch("http://e/gl.onelf", &tmp).unwrap_err();
        assert!(err.contains("https:// or file://"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
