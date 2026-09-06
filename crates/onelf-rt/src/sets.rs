//! Dependency sets a package shares with others instead of bundling.
//!
//! A set is the closure of some sysroot packages, packed as a build and
//! pinned by hash in `.onelf/sets`. The bundle carries none of it, so a
//! set is not conditional the way the GL build is: every set the package
//! names has to be there, and a set that cannot be obtained is a failed
//! launch rather than a degraded one.
//!
//! Obtaining one is the GL build's path exactly, so two packages naming
//! the same set share one download and one extraction.

use std::fs;
use std::path::{Path, PathBuf};

use crate::platform::Pin;

/// Where the package records the sets it needs, relative to the package
/// root.
pub const SETS_FILE: &str = ".onelf/sets";

/// One obtained set: the extracted root and the lock that keeps the
/// extraction alive for this instance.
pub struct Obtained {
    pub root: PathBuf,
    pub _lock: fs::File,
}

/// The sets the record names, in file order. A malformed table is
/// skipped rather than guessed at; the launch then fails on the missing
/// libraries, which names the problem better than a parse error would.
pub fn parse(text: &str) -> Vec<Pin> {
    let mut out: Vec<Pin> = Vec::new();
    let mut name: Option<String> = None;
    let (mut url, mut blake3) = (None, None);
    let flush = |name: &mut Option<String>,
                 url: &mut Option<String>,
                 blake3: &mut Option<String>,
                 out: &mut Vec<Pin>| {
        if let (Some(label), Some(url), Some(blake3)) = (name.take(), url.take(), blake3.take())
            && let Some(pin) = crate::platform::checked_pin(label, url, blake3)
        {
            out.push(pin);
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[') {
            flush(&mut name, &mut url, &mut blake3, &mut out);
            name = rest.strip_suffix(']').map(|n| n.trim().to_string());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let Some(value) = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .map(|v| v.replace("\\\"", "\"").replace("\\\\", "\\"))
        else {
            continue;
        };
        match key.trim() {
            "url" => url = Some(value),
            "blake3" => blake3 = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    flush(&mut name, &mut url, &mut blake3, &mut out);
    out
}

/// Obtain every set the package at `pkg_root` names. The error names the
/// first set that could not be had, since the bundle cannot run without
/// what it left out.
pub fn obtain_all(pkg_root: &Path) -> Result<Vec<Obtained>, String> {
    let Ok(text) = fs::read_to_string(pkg_root.join(SETS_FILE)) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for pin in parse(&text) {
        let (root, lock) = crate::platform::obtain_build(&pin)
            .map_err(|why| format!("dependency set {}: {why}", pin.label))?;
        out.push(Obtained { root, _lock: lock });
    }
    Ok(out)
}

/// The library directories a set root carries, in search order.
pub fn lib_dirs(root: &Path) -> Vec<PathBuf> {
    onelf_format::resolve::GL_BUILD_LIB_DIRS
        .iter()
        .map(|d| root.join(d))
        .filter(|d| d.is_dir())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_yields_one_pin_per_table() {
        let hash = "ab".repeat(32);
        let text = format!(
            "[qt]\nurl = \"file:///a/qt.onelf\"\nblake3 = \"{hash}\"\npackages = [\"qt6-base\"]\n\n\
             [gtk]\nurl = \"https://e/gtk.onelf\"\nblake3 = \"{hash}\"\npackages = []\n"
        );
        let pins = parse(&text);
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].label, "qt");
        assert_eq!(pins[0].url, "file:///a/qt.onelf");
        assert_eq!(pins[1].label, "gtk");
        assert_eq!(pins[1].blake3, hash);
    }

    #[test]
    fn a_table_missing_a_field_or_carrying_a_bad_one_is_skipped() {
        let hash = "cd".repeat(32);
        let text = format!(
            "[nourl]\nblake3 = \"{hash}\"\n\n\
             [badhash]\nurl = \"file:///a\"\nblake3 = \"abc\"\n\n\
             [badname/x]\nurl = \"file:///a\"\nblake3 = \"{hash}\"\n\n\
             [ok]\nurl = \"file:///a\"\nblake3 = \"{hash}\"\n"
        );
        let pins = parse(&text);
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].label, "ok");
    }
}
