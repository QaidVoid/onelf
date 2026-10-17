//! What a launch changed in the environment, recorded so that a program
//! outside the package starts with the environment the runtime was
//! started with.
//!
//! The runtime changes the environment for the application, and every
//! process the application starts inherits it. The exec and spawn hooks
//! of the `onelf-env` constructor read the record made here and give a
//! host program the original values back; see `onelf-payloads/src/exec.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The record of what the runtime changed, read by the `onelf-env` exec
/// hooks. See [`record_host_env`] for the format.
const HOST_ENV_VAR: &str = "ONELF_HOST_ENV";
/// The package root and the dependency set roots, canonical and
/// `:`-separated: a program below one of them is the package's own.
const ROOTS_VAR: &str = "ONELF_ROOTS";

/// The environment this runtime was started with, after
/// [`capture_starting_env`] cleaned it.
static STARTING_ENV: OnceLock<BTreeMap<OsString, OsString>> = OnceLock::new();

/// Variables naming modules, drivers or data that a library loads into
/// the process reading them. A value pointing into another onelf package
/// is that package's, and is never right for this one.
const MODULE_VARS: &[&str] = &[
    "QT_PLUGIN_PATH",
    "GIO_EXTRA_MODULES",
    "GDK_PIXBUF_MODULE_FILE",
    "GTK_IM_MODULE_FILE",
    "GTK_PATH",
    "GCONV_PATH",
    "FONTCONFIG_FILE",
    "LIBDECOR_PLUGIN_DIR",
    "LIBGL_DRIVERS_PATH",
    "LIBVA_DRIVERS_PATH",
    "GBM_BACKENDS_PATH",
    "__EGL_VENDOR_LIBRARY_DIRS",
    "VK_DRIVER_FILES",
    "VK_ADD_DRIVER_FILES",
    "DRIRC_CONFIGDIR",
    "LIBDRM_IDS_PATH",
    "XKB_CONFIG_ROOT",
];

/// Clean the environment this runtime was started with and remember it,
/// before anything else here changes it.
///
/// A process started by a packed application normally gets the caller's
/// environment back from the exec hooks. One started some way the hooks
/// cannot see (a static binary, `system`, `execl`) still carries the
/// launching package's record, which is applied here instead. A module
/// variable that still points into another package after that is
/// dropped.
pub fn capture_starting_env() {
    if let Some(record) = env::var_os(HOST_ENV_VAR) {
        for entry in parse_record(record.as_bytes()) {
            if !entry.restores(env::var_os(&entry.name).as_deref()) || entry.name.is_empty() {
                continue;
            }
            // SAFETY: the runtime is single-threaded at this point
            unsafe {
                match &entry.original {
                    Some(value) => env::set_var(&entry.name, value),
                    None => env::remove_var(&entry.name),
                }
            }
        }
        unsafe { env::remove_var(HOST_ENV_VAR) };
    }
    for var in MODULE_VARS {
        let foreign = env::var_os(var).is_some_and(|value| {
            value
                .as_bytes()
                .split(|&b| b == b':')
                .any(|p| in_onelf_package(Path::new(OsStr::from_bytes(p))))
        });
        if foreign {
            unsafe { env::remove_var(var) };
        }
    }
    let _ = STARTING_ENV.set(env::vars_os().collect());
}

/// Whether `path` lies in an onelf package tree, or in what a runtime
/// generated for one under the private base.
fn in_onelf_package(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path.ancestors().any(|dir| dir.join(".onelf").is_dir()) {
        return true;
    }
    let Some(base) = crate::paths::private_dir() else {
        return false;
    };
    path.strip_prefix(&base)
        .ok()
        .and_then(|rest| rest.components().next())
        .and_then(|first| first.as_os_str().to_str())
        .is_some_and(|first| {
            ["onelf-", "modules-", "fontconfig-"]
                .iter()
                .any(|prefix| first.starts_with(prefix))
        })
}

/// Record, for the exec hooks of the `onelf-env` constructor, what this
/// launch changed in the environment, so that a program outside the
/// package starts with the environment the runtime was started with.
///
/// `ONELF_ROOTS` gets the canonical package and set roots, and
/// `ONELF_HOST_ENV` one entry per variable whose value differs from the
/// starting one:
///
/// ```text
/// NAME=<applied><original>;
/// applied:  16 hex digits    FNV-1a hash of the value set here
///           -                the variable was removed here
///           *                restore whatever the variable holds
/// original: <len>:<bytes>    the starting value, <len> bytes long
///           -                the variable was unset
/// ```
///
/// A hook restores a variable only while it holds what was applied, so
/// a value the application changes on purpose survives.
pub fn record_host_env(pkg_root: &Path, set_roots: &[PathBuf]) {
    let Some(start) = STARTING_ENV.get() else {
        return;
    };
    let roots: Vec<PathBuf> = std::iter::once(pkg_root)
        .chain(set_roots.iter().map(PathBuf::as_path))
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()))
        .collect();
    if let Ok(joined) = env::join_paths(roots) {
        // SAFETY: the runtime is single-threaded at this point
        unsafe { env::set_var(ROOTS_VAR, joined) };
    }
    let now: BTreeMap<OsString, OsString> = env::vars_os().collect();
    let record = build_record(start, &now);
    unsafe { env::set_var(HOST_ENV_VAR, OsString::from_vec(record)) };
}

/// Set on the exec command itself after the record is made, so whatever
/// they hold is restored: `ONELF_INTERP` by a launch under the host
/// loader, and `LD_LIBRARY_PATH` by the fallback for an entrypoint
/// without an `$ORIGIN` run path.
const SET_AT_EXEC: &[&str] = &["ONELF_INTERP", "LD_LIBRARY_PATH"];

/// One entry for every variable that differs between `start` and `now`,
/// and one for each of [`SET_AT_EXEC`].
fn build_record(
    start: &BTreeMap<OsString, OsString>,
    now: &BTreeMap<OsString, OsString>,
) -> Vec<u8> {
    let at_exec: Vec<OsString> = SET_AT_EXEC.iter().map(OsString::from).collect();
    let names: BTreeSet<&OsString> = start
        .keys()
        .chain(now.keys())
        .chain(at_exec.iter())
        .filter(|name| *name != HOST_ENV_VAR)
        .collect();
    let mut record = Vec::new();
    for name in names {
        let (before, after) = (start.get(name), now.get(name));
        let applied = if at_exec.contains(name) {
            Applied::Any
        } else if before == after {
            continue;
        } else {
            after.map_or(Applied::Removed, |v| Applied::Value(fnv1a(v.as_bytes())))
        };
        encode_entry(&mut record, name, applied, before.map(OsString::as_os_str));
    }
    record
}

/// What the runtime did to a recorded variable.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Applied {
    Removed,
    Value(u64),
    Any,
}

#[derive(Debug, PartialEq)]
struct RecordEntry {
    name: OsString,
    applied: Applied,
    original: Option<OsString>,
}

impl RecordEntry {
    /// Whether a variable now holding `current` still holds what the
    /// runtime applied.
    fn restores(&self, current: Option<&OsStr>) -> bool {
        match (self.applied, current) {
            (Applied::Any, _) | (Applied::Removed, None) => true,
            (Applied::Value(hash), Some(value)) => fnv1a(value.as_bytes()) == hash,
            _ => false,
        }
    }
}

fn encode_entry(out: &mut Vec<u8>, name: &OsStr, applied: Applied, original: Option<&OsStr>) {
    out.extend_from_slice(name.as_bytes());
    out.push(b'=');
    match applied {
        Applied::Removed => out.push(b'-'),
        Applied::Any => out.push(b'*'),
        Applied::Value(hash) => out.extend_from_slice(format!("{hash:016x}").as_bytes()),
    }
    match original {
        None => out.push(b'-'),
        Some(value) => {
            out.extend_from_slice(format!("{}:", value.len()).as_bytes());
            out.extend_from_slice(value.as_bytes());
        }
    }
    out.push(b';');
}

/// The entries of a record, up to the first malformed one.
fn parse_record(mut rest: &[u8]) -> Vec<RecordEntry> {
    let mut entries = Vec::new();
    while let Some((entry, tail)) = parse_entry(rest) {
        entries.push(entry);
        rest = tail;
    }
    entries
}

fn parse_entry(text: &[u8]) -> Option<(RecordEntry, &[u8])> {
    let eq = text.iter().position(|&b| b == b'=')?;
    let (name, mut rest) = (&text[..eq], &text[eq + 1..]);
    let applied = match rest.first()? {
        b'-' => {
            rest = &rest[1..];
            Applied::Removed
        }
        b'*' => {
            rest = &rest[1..];
            Applied::Any
        }
        _ => {
            let hex = std::str::from_utf8(rest.get(..16)?).ok()?;
            if !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return None;
            }
            rest = &rest[16..];
            Applied::Value(u64::from_str_radix(hex, 16).ok()?)
        }
    };
    let original = if rest.first() == Some(&b'-') {
        rest = &rest[1..];
        None
    } else {
        let colon = rest.iter().position(|&b| b == b':')?;
        let len: usize = std::str::from_utf8(&rest[..colon]).ok()?.parse().ok()?;
        let value = rest.get(colon + 1..colon + 1 + len)?;
        rest = &rest[colon + 1 + len..];
        Some(OsString::from_vec(value.to_vec()))
    };
    let rest = rest.strip_prefix(b";")?;
    let entry = RecordEntry {
        name: OsString::from_vec(name.to_vec()),
        applied,
        original,
    };
    Some((entry, rest))
}

/// 64-bit FNV-1a, which the constructor computes the same way.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn a_record_lists_what_changed_and_parses_back() {
        let start = map(&[("HOME", "/home/a"), ("KEPT", "same"), ("GONE", "x=1;y")]);
        let now = map(&[
            ("HOME", "/portable"),
            ("KEPT", "same"),
            ("ONELF_DIR", "/pkg"),
            (HOST_ENV_VAR, "stale"),
        ]);
        let entries = parse_record(&build_record(&start, &now));
        let expect = [
            RecordEntry {
                name: "GONE".into(),
                applied: Applied::Removed,
                original: Some("x=1;y".into()),
            },
            RecordEntry {
                name: "HOME".into(),
                applied: Applied::Value(fnv1a(b"/portable")),
                original: Some("/home/a".into()),
            },
            RecordEntry {
                name: "LD_LIBRARY_PATH".into(),
                applied: Applied::Any,
                original: None,
            },
            RecordEntry {
                name: "ONELF_DIR".into(),
                applied: Applied::Value(fnv1a(b"/pkg")),
                original: None,
            },
            RecordEntry {
                name: "ONELF_INTERP".into(),
                applied: Applied::Any,
                original: None,
            },
        ];
        assert_eq!(entries, expect);
    }

    #[test]
    fn a_value_changed_after_the_launch_is_not_restored() {
        let entry = RecordEntry {
            name: "PATH".into(),
            applied: Applied::Value(fnv1a(b"/pkg/bin:/usr/bin")),
            original: Some("/usr/bin".into()),
        };
        assert!(entry.restores(Some(OsStr::new("/pkg/bin:/usr/bin"))));
        assert!(!entry.restores(Some(OsStr::new("/extra:/pkg/bin:/usr/bin"))));
        assert!(!entry.restores(None));

        let removed = RecordEntry {
            applied: Applied::Removed,
            ..entry
        };
        assert!(removed.restores(None));
        assert!(!removed.restores(Some(OsStr::new("set again"))));
    }

    #[test]
    fn a_malformed_record_ends_at_the_bad_entry() {
        let mut record = Vec::new();
        encode_entry(
            &mut record,
            OsStr::new("A"),
            Applied::Removed,
            Some(OsStr::new("1")),
        );
        record.extend_from_slice(b"B=zz-;C=--;");
        assert_eq!(parse_record(&record).len(), 1);
        assert!(parse_record(b"A=-99:short;").is_empty());
    }

    /// The constructor computes the same hash; a change here has to be
    /// made there too.
    #[test]
    fn the_hash_is_fnv1a() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
