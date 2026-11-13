//! The descriptor anchor record, `.onelf/anchors`.
//!
//! A library compiled to open an absolute directory, `/usr/share/name`
//! say, can have that string rewritten in its bundled copy so it opens
//! `/dev/fd/77/name` instead. At launch the runtime opens descriptor 77
//! on a private per-launch directory holding one symlink per key, each
//! naming the bundle directory the key stands for. `/dev/fd/77` resolves
//! through the opening process's own descriptor table, so no other
//! process or user can create or replace what it names, and it needs no
//! namespace.
//!
//! The rewrite replaces only a path's first [`ANCHOR_PREFIX_LEN`] bytes
//! with [`ANCHOR_PREFIX`] and keeps the rest, so the key is the original
//! path's tail ([`key_of`]). The string keeps its length and needs no
//! padding, a library that measures it rather than reading up to its
//! terminator still sees a whole path, and a shorter string the linker
//! stored inside the tail, such as `"input"` inside
//! `"/usr/share/libinput"`, keeps its bytes. A path below the directory
//! takes the same rewrite and resolves through the same key.
//!
//! Each line pairs a key with the bundle directory it stands for, as
//! `KEY<tab>RELATIVE`, with lines separated by a single `\n` and no
//! trailing newline. The keys are listed in byte order, each once, and no
//! key lies below another one: a path below a key already resolves
//! through that key's link. Keys and directories follow the path overlay
//! record's rules for a bundle path: relative, normalized and free of
//! control characters. Anything else is refused as a whole.
//!
//! The descriptor is an ordinary inherited one, which shapes three edge
//! cases:
//!
//! - A descriptor 77 the launch was started with is replaced. Whatever
//!   the caller had open there is closed in the launched application, as
//!   `exec 77<file` in a shell would close it.
//! - A bundled program started without descriptor 77, as Python's
//!   `subprocess`, GLib's spawn functions and Chromium's launcher do by
//!   closing every inherited descriptor, reopens it from the path in
//!   [`ANCHOR_DIR_ENV`] before its own code runs.
//! - A process that has neither, such as a host program or one started
//!   with a cleared environment, resolves no anchor: every anchored path
//!   fails as a missing file. It never resolves somewhere else, because
//!   `/dev/fd/77` then names no descriptor at all.

use std::collections::HashSet;
use std::fmt;

use crate::paths::{self, PathsErrorKind};

/// Where a package carries the record.
pub const ANCHORS_FILE: &str = ".onelf/anchors";
/// The descriptor the runtime holds the anchor directory open on.
/// Shell scripts take low numbers such as 9 for their own redirections,
/// so a high one is left alone by them.
pub const ANCHOR_FD: i32 = 77;
/// The directory an anchored path starts with.
pub const ANCHOR_ROOT: &str = "/dev/fd/77";
/// What a rewrite puts over the start of a compiled-in path.
pub const ANCHOR_PREFIX: &str = "/dev/fd/77/";
/// The bytes of a compiled-in path a rewrite replaces.
pub const ANCHOR_PREFIX_LEN: usize = ANCHOR_PREFIX.len();
/// The environment variable naming the anchor directory, so a bundled
/// program started without descriptor 77 can reopen it.
pub const ANCHOR_DIR_ENV: &str = "ONELF_ANCHOR_DIR";
/// The most anchors one record holds.
pub const MAX_ANCHORS: usize = 1024;
/// The largest complete record, in bytes, checked before anything is
/// parsed or allocated.
pub const MAX_RECORD_BYTES: usize = 256 * 1024;

/// One anchor: its key and the bundle directory, relative to the package
/// root, that `/dev/fd/77/<key>` resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// The key: the anchored path below the anchor root.
    pub key: String,
    /// The bundle directory, relative to the package root.
    pub relative: String,
}

impl Anchor {
    /// The path a rewritten string names: `/dev/fd/77/<key>`.
    pub fn path(&self) -> String {
        format!("{ANCHOR_ROOT}/{}", self.key)
    }
}

/// The key a compiled-in path takes, or `None` when it cannot take one:
/// the bytes after its first [`ANCHOR_PREFIX_LEN`], without the slash a
/// component boundary there leaves, when they form a path a record may
/// name. A path no longer than the prefix leaves nothing to key on.
pub fn key_of(spelling: &str) -> Option<String> {
    if spelling.len() <= ANCHOR_PREFIX_LEN || !spelling.is_char_boundary(ANCHOR_PREFIX_LEN) {
        return None;
    }
    let key = spelling[ANCHOR_PREFIX_LEN..].trim_start_matches('/');
    paths::relative_error(key).is_none().then(|| key.to_owned())
}

/// Whether `key` lies strictly below `parent`, component by component.
pub fn key_below(key: &str, parent: &str) -> bool {
    key.len() > parent.len() && key.starts_with(parent) && key.as_bytes()[parent.len()] == b'/'
}

/// Why a record or an anchor was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorsErrorKind {
    /// The record is over [`MAX_RECORD_BYTES`].
    RecordTooLarge,
    /// The record holds more than [`MAX_ANCHORS`] anchors.
    TooManyAnchors,
    /// The record is not UTF-8.
    NotUtf8,
    /// A line holds no anchor: a blank line, or a trailing newline.
    EmptyLine,
    /// A line has no tab between its key and its directory.
    MissingSeparator,
    /// A line has a tab after its directory.
    TrailingField,
    /// A key is not one a record may hold.
    Key(PathsErrorKind),
    /// A key does not sort after the one before it, or repeats it.
    Unsorted,
    /// A key lies below another key of the record.
    Nested,
    /// The bundle directory is not one a record may name.
    Directory(PathsErrorKind),
}

/// A refused record or anchor, naming its line when there is one,
/// numbered from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorsError {
    kind: AnchorsErrorKind,
    line: Option<usize>,
}

impl AnchorsError {
    fn record(kind: AnchorsErrorKind) -> Self {
        Self { kind, line: None }
    }

    fn line(kind: AnchorsErrorKind, index: usize) -> Self {
        Self {
            kind,
            line: Some(index + 1),
        }
    }

    /// Why the record was refused.
    pub fn kind(&self) -> AnchorsErrorKind {
        self.kind
    }
}

impl fmt::Display for AnchorsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.kind {
            AnchorsErrorKind::RecordTooLarge => {
                format!("the record is over {MAX_RECORD_BYTES} bytes")
            }
            AnchorsErrorKind::TooManyAnchors => {
                format!("the record holds over {MAX_ANCHORS} anchors")
            }
            AnchorsErrorKind::NotUtf8 => "the record is not UTF-8".into(),
            AnchorsErrorKind::EmptyLine => "the line holds no anchor".into(),
            AnchorsErrorKind::MissingSeparator => {
                "the line has no tab between its key and directory".into()
            }
            AnchorsErrorKind::TrailingField => "the line goes on after the directory".into(),
            AnchorsErrorKind::Key(kind) => format!("the key: {}", kind.reason()),
            AnchorsErrorKind::Unsorted => "the key does not sort after the one before it".into(),
            AnchorsErrorKind::Nested => "the key lies below another key".into(),
            AnchorsErrorKind::Directory(kind) => kind.reason(),
        };
        match self.line {
            Some(number) => write!(f, "line {number}: {reason}"),
            None => f.write_str(&reason),
        }
    }
}

impl std::error::Error for AnchorsError {}

/// Encode anchors as a record, refusing any record [`decode`] would refuse.
pub fn encode(anchors: &[Anchor]) -> Result<Vec<u8>, AnchorsError> {
    if anchors.len() > MAX_ANCHORS {
        return Err(AnchorsError::record(AnchorsErrorKind::TooManyAnchors));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (index, anchor) in anchors.iter().enumerate() {
        let previous = index.checked_sub(1).map(|i| anchors[i].key.as_str());
        check_anchor(index, &anchor.key, &anchor.relative, previous, &seen)?;
        seen.insert(anchor.key.as_str());
        if index > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(anchor.key.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(anchor.relative.as_bytes());
        if out.len() > MAX_RECORD_BYTES {
            return Err(AnchorsError::record(AnchorsErrorKind::RecordTooLarge));
        }
    }
    Ok(out)
}

/// Decode a record into its anchors, in record order.
///
/// The size is checked before the bytes are looked at, so nothing
/// allocated here is larger than [`MAX_RECORD_BYTES`] allows.
pub fn decode(bytes: &[u8]) -> Result<Vec<Anchor>, AnchorsError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(AnchorsError::record(AnchorsErrorKind::RecordTooLarge));
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| AnchorsError::record(AnchorsErrorKind::NotUtf8))?;
    let mut anchors: Vec<Anchor> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for (index, line) in text.split('\n').enumerate() {
        if index >= MAX_ANCHORS {
            return Err(AnchorsError::record(AnchorsErrorKind::TooManyAnchors));
        }
        if line.is_empty() {
            return Err(AnchorsError::line(AnchorsErrorKind::EmptyLine, index));
        }
        let Some((key, relative)) = line.split_once('\t') else {
            return Err(AnchorsError::line(
                AnchorsErrorKind::MissingSeparator,
                index,
            ));
        };
        if relative.contains('\t') {
            return Err(AnchorsError::line(AnchorsErrorKind::TrailingField, index));
        }
        let previous = anchors.last().map(|a| a.key.as_str());
        check_anchor(index, key, relative, previous, &seen)?;
        seen.insert(key);
        anchors.push(Anchor {
            key: key.to_owned(),
            relative: relative.to_owned(),
        });
    }
    Ok(anchors)
}

/// Check the anchor at `index`: its key is one a record may hold, sorts
/// after `previous` and lies below none of the keys in `seen`, and its
/// directory is one a record may name.
fn check_anchor(
    index: usize,
    key: &str,
    relative: &str,
    previous: Option<&str>,
    seen: &HashSet<&str>,
) -> Result<(), AnchorsError> {
    if let Some(kind) = paths::relative_error(key) {
        return Err(AnchorsError::line(AnchorsErrorKind::Key(kind), index));
    }
    if let Some(kind) = paths::relative_error(relative) {
        return Err(AnchorsError::line(AnchorsErrorKind::Directory(kind), index));
    }
    if previous.is_some_and(|previous| key <= previous) {
        return Err(AnchorsError::line(AnchorsErrorKind::Unsorted, index));
    }
    let mut ancestor = key;
    while let Some((parent, _)) = ancestor.rsplit_once('/') {
        if seen.contains(parent) {
            return Err(AnchorsError::line(AnchorsErrorKind::Nested, index));
        }
        ancestor = parent;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(bytes: &[u8]) -> AnchorsErrorKind {
        decode(bytes).unwrap_err().kind()
    }

    fn anchor(key: &str, relative: &str) -> Anchor {
        Anchor {
            key: key.into(),
            relative: relative.into(),
        }
    }

    #[test]
    fn a_key_is_the_tail_after_the_prefix() {
        assert_eq!(key_of("/usr/share/libinput").as_deref(), Some("libinput"));
        assert_eq!(key_of("/usr/lib64/alsa-lib").as_deref(), Some("alsa-lib"));
        assert_eq!(key_of("/usr/lib/alsa-lib").as_deref(), Some("sa-lib"));
        assert_eq!(key_of("/usr/share/X11/xkb").as_deref(), Some("X11/xkb"));
        assert_eq!(key_of("/opt/vendor/lib/fw").as_deref(), Some("lib/fw"));
        let spelling = "/usr/share/libinput";
        let rewritten = format!("{ANCHOR_PREFIX}{}", &spelling[ANCHOR_PREFIX_LEN..]);
        assert_eq!(
            rewritten.len(),
            spelling.len(),
            "the string keeps its length"
        );
        assert_eq!(rewritten, anchor("libinput", "share/libinput").path());
    }

    #[test]
    fn a_path_without_a_usable_tail_takes_no_key() {
        for spelling in [
            "/usr/lib/x",
            "/usr/share/",
            "/usr/lib/a..",
            "/usr/lib/ab/.",
            "/usr/lib/ab/../c",
        ] {
            assert_eq!(key_of(spelling), None, "{spelling}");
        }
        assert_eq!(key_of("/usr/lib/ab/c").as_deref(), Some("c"));
        assert_eq!(key_of("/usr/lib/abc/d").as_deref(), Some("c/d"));
    }

    #[test]
    fn nesting_is_by_component() {
        assert!(key_below("alsa/ucm2", "alsa"));
        assert!(!key_below("alsa-lib", "alsa"));
        assert!(!key_below("alsa", "alsa"));
    }

    #[test]
    fn a_record_round_trips() {
        let anchors = vec![
            anchor("X11/locale", "share/X11/locale"),
            anchor("X11/xkb", "share/xkeyboard-config-2"),
            anchor("alsa", "share/alsa"),
            anchor("xkeyboard-config-2", "share/xkeyboard-config-2"),
        ];
        let bytes = encode(&anchors).unwrap();
        assert_eq!(
            bytes,
            b"X11/locale\tshare/X11/locale\nX11/xkb\tshare/xkeyboard-config-2\nalsa\tshare/alsa\nxkeyboard-config-2\tshare/xkeyboard-config-2"
        );
        assert_eq!(decode(&bytes).unwrap(), anchors);
        assert_eq!(decode(b"").unwrap(), Vec::new());
    }

    #[test]
    fn a_record_the_packer_would_not_write_is_refused() {
        let cases: &[(&[u8], AnchorsErrorKind)] = &[
            (b"a\tshare/x\n", AnchorsErrorKind::EmptyLine),
            (b"a\tshare/x\n\nb\tshare/y", AnchorsErrorKind::EmptyLine),
            (b"a share/x", AnchorsErrorKind::MissingSeparator),
            (b"a\tshare/x\textra", AnchorsErrorKind::TrailingField),
            (b"b\tshare/x\na\tshare/y", AnchorsErrorKind::Unsorted),
            (b"a\tshare/x\na\tshare/y", AnchorsErrorKind::Unsorted),
            (b"a\tshare/x\na/b\tshare/y", AnchorsErrorKind::Nested),
            (
                b"a\tshare/x\na-b\tshare/y\na/b\tshare/z",
                AnchorsErrorKind::Nested,
            ),
            (
                b"/a\tshare/x",
                AnchorsErrorKind::Key(PathsErrorKind::RelativeIsAbsolute),
            ),
            (
                b"a/../b\tshare/x",
                AnchorsErrorKind::Key(PathsErrorKind::Escapes),
            ),
            (
                b"a//b\tshare/x",
                AnchorsErrorKind::Key(PathsErrorKind::Unnormalized),
            ),
            (b"\xff\tshare/x", AnchorsErrorKind::NotUtf8),
        ];
        for (bytes, kind) in cases {
            assert_eq!(refused(bytes), *kind, "{bytes:?}");
        }
        assert_eq!(
            encode(&[anchor("a", "share/x"), anchor("a/b", "share/y")])
                .unwrap_err()
                .kind(),
            AnchorsErrorKind::Nested
        );
    }

    #[test]
    fn a_directory_outside_the_package_or_unnormalized_is_refused() {
        for (relative, kind) in [
            ("/usr/share/x", PathsErrorKind::RelativeIsAbsolute),
            ("../x", PathsErrorKind::Escapes),
            ("share/../../x", PathsErrorKind::Escapes),
            ("share//x", PathsErrorKind::Unnormalized),
            ("share/x/", PathsErrorKind::Unnormalized),
            ("./share", PathsErrorKind::Unnormalized),
            ("", PathsErrorKind::Unnormalized),
            ("share/\u{1}x", PathsErrorKind::ControlCharacter),
        ] {
            let line = format!("a\t{relative}");
            assert_eq!(
                refused(line.as_bytes()),
                AnchorsErrorKind::Directory(kind),
                "{relative:?}"
            );
            assert_eq!(
                encode(&[anchor("a", relative)]).unwrap_err().kind(),
                AnchorsErrorKind::Directory(kind),
                "{relative:?}"
            );
        }
        let long = format!("a\t{}", "x".repeat(paths::MAX_RELATIVE_BYTES + 1));
        assert_eq!(
            refused(long.as_bytes()),
            AnchorsErrorKind::Directory(PathsErrorKind::RelativeTooLong)
        );
    }

    #[test]
    fn the_bounds_are_checked_first() {
        let huge = vec![b'a'; MAX_RECORD_BYTES + 1];
        assert_eq!(refused(&huge), AnchorsErrorKind::RecordTooLarge);
        let anchors: Vec<Anchor> = (0..=MAX_ANCHORS)
            .map(|i| anchor(&format!("k{i:05}"), &format!("d/{i:05}")))
            .collect();
        assert_eq!(
            encode(&anchors).unwrap_err().kind(),
            AnchorsErrorKind::TooManyAnchors
        );
        let lines: Vec<String> = anchors
            .iter()
            .map(|a| format!("{}\t{}", a.key, a.relative))
            .collect();
        assert_eq!(
            refused(lines.join("\n").as_bytes()),
            AnchorsErrorKind::TooManyAnchors
        );
        assert!(encode(&anchors[..MAX_ANCHORS]).is_ok());
    }

    #[test]
    fn errors_name_the_line() {
        let err = decode(b"a\tshare/x\nb\t../y").unwrap_err();
        assert_eq!(
            err.to_string(),
            "line 2: the bundle path leaves the package"
        );
    }
}
