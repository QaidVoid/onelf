//! The GL build a sysroot names for hosts that have none.
//!
//! A sysroot may carry `etc/onelf/platform.toml`:
//!
//! ```toml
//! label = "platform-1"
//!
//! [gl]
//! url = "https://example.com/platform-1/gl.onelf"
//! blake3 = "<64 hex characters>"
//! ```
//!
//! A package built on the sysroot pins those three values, so the
//! publisher who built on the sysroot is the trust root and nothing has
//! to be signed.

use std::io;
use std::path::Path;

/// Where the pin sits under a sysroot root.
pub const PLATFORM_FILE: &str = "etc/onelf/platform.toml";

/// A GL build pinned by hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub label: String,
    pub url: String,
    /// Lowercase hex BLAKE3 of the build's onelf package.
    pub blake3: String,
}

fn invalid(root: &Path, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {PLATFORM_FILE}: {what}", root.display()),
    )
}

/// The pin the sysroot at `root` names, or `None` when it names none.
pub fn read(root: &Path) -> io::Result<Option<Pin>> {
    let path = root.join(PLATFORM_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    parse(&text).map(Some).map_err(|what| invalid(root, &what))
}

/// Parse the file's text. Public so a recipe override can be validated
/// with the same rules.
pub fn parse(text: &str) -> Result<Pin, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let label = value
        .get("label")
        .and_then(toml::Value::as_str)
        .ok_or("no label")?
        .trim()
        .to_string();
    if label.is_empty() || label.contains('/') {
        return Err("label must be a non-empty name without slashes".into());
    }
    let gl = value.get("gl").ok_or("no [gl] table")?;
    let url = gl
        .get("url")
        .and_then(toml::Value::as_str)
        .ok_or("no gl.url")?
        .to_string();
    let blake3 = gl
        .get("blake3")
        .and_then(toml::Value::as_str)
        .ok_or("no gl.blake3")?
        .to_ascii_lowercase();
    check_hash(&blake3)?;
    check_url(&url)?;
    Ok(Pin { label, url, blake3 })
}

/// A BLAKE3 hash is 64 hex characters, nothing else.
pub fn check_hash(hash: &str) -> Result<(), String> {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{hash}: not a BLAKE3 hash (64 hex characters)"));
    }
    Ok(())
}

/// A build is executable code, so it comes over TLS or from a local
/// file, never plain HTTP.
pub fn check_url(url: &str) -> Result<(), String> {
    if url.starts_with("https://") || url.starts_with("file://") {
        Ok(())
    } else {
        Err(format!(
            "{url}: the GL build must be an https:// or file:// URL"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::fixture::temp_root;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn reads_a_complete_file() {
        let root = temp_root("platform");
        std::fs::create_dir_all(root.join("etc/onelf")).unwrap();
        std::fs::write(
            root.join(PLATFORM_FILE),
            format!("label = \"platform-1\"\n\n[gl]\nurl = \"https://example.com/gl.onelf\"\nblake3 = \"{}\"\n", HASH.to_ascii_uppercase()),
        )
        .unwrap();
        let pin = read(&root).unwrap().unwrap();
        assert_eq!(pin.label, "platform-1");
        assert_eq!(pin.url, "https://example.com/gl.onelf");
        assert_eq!(pin.blake3, HASH, "hash is normalized to lowercase");
    }

    #[test]
    fn an_absent_file_is_no_pin() {
        let root = temp_root("noplatform");
        assert_eq!(read(&root).unwrap(), None);
    }

    #[test]
    fn a_truncated_file_names_the_sysroot() {
        let root = temp_root("badplatform");
        std::fs::create_dir_all(root.join("etc/onelf")).unwrap();
        std::fs::write(
            root.join(PLATFORM_FILE),
            "label = \"x\"\n[gl]\nurl = \"https://e/gl\"\nblake3 = \"012",
        )
        .unwrap();
        let err = read(&root).unwrap_err();
        assert!(err.to_string().contains("etc/onelf/platform.toml"), "{err}");
    }

    #[test]
    fn schemes_and_hashes_are_checked() {
        assert!(check_url("http://example.com/gl.onelf").is_err());
        assert!(check_url("file:///srv/gl.onelf").is_ok());
        assert!(check_hash("abc").is_err());
        assert!(check_hash(HASH).is_ok());
        let text = format!("label = \"a/b\"\n[gl]\nurl = \"https://e/gl\"\nblake3 = \"{HASH}\"\n");
        assert!(parse(&text).is_err(), "a label is a directory name");
    }
}
