//! The descriptor anchor record, `.onelf/anchors`.
//!
//! A library compiled to open an absolute directory, `/usr/share/name`
//! say, can have that string rewritten in its bundled copy to
//! `/dev/fd/77/<key>`. At launch the runtime opens descriptor 77 on a
//! private per-launch directory holding one symlink per key, each naming
//! the bundle directory the key stands for. `/dev/fd/77` resolves through
//! the opening process's own descriptor table, so no other process or
//! user can create or replace what it names, and it needs no namespace.
//!
//! Each line pairs a key with the bundle directory it stands for, as
//! `KEY<tab>RELATIVE`, with lines separated by a single `\n` and no
//! trailing newline. The directories are listed in byte order, each once,
//! and the keys are [`key`] of the line's index, so the shortest names go
//! to the first directories and a given set of directories always yields
//! the same record. The bundle side follows the same rules as the path
//! overlay record's: relative, normalized, inside the package and free of
//! control characters. Anything else is refused as a whole.
//!
//! The descriptor is an ordinary inherited one, which shapes two edge
//! cases:
//!
//! - A descriptor 77 the launch was started with is replaced. Whatever
//!   the caller had open there is closed in the launched application, as
//!   `exec 77<file` in a shell would close it.
//! - A process that closes descriptor 77, or a child started without it,
//!   resolves no anchor: every anchored path fails as a missing file. It
//!   never resolves somewhere else, because `/dev/fd/77` then names no
//!   descriptor at all.

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
/// The most anchors one record holds.
pub const MAX_ANCHORS: usize = 1024;
/// The largest complete record, in bytes, checked before anything is
/// parsed or allocated.
pub const MAX_RECORD_BYTES: usize = 256 * 1024;

/// One anchor: its key and the bundle directory, relative to the package
/// root, that `/dev/fd/77/<key>` resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// The key, the anchored path's last component.
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

/// The key of the anchor at `index`: `a` to `z`, then `aa`, `ab` and so
/// on, so each key is as short as the count before it allows.
pub fn key(index: usize) -> String {
    let mut n = index + 1;
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(b'a' + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
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
    /// A key is not the one its position assigns.
    WrongKey,
    /// A directory does not sort after the one before it, or repeats it.
    Unsorted,
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
            AnchorsErrorKind::WrongKey => "the key is not the one its position assigns".into(),
            AnchorsErrorKind::Unsorted => {
                "the directory does not sort after the one before it".into()
            }
            AnchorsErrorKind::Directory(kind) => kind.reason(),
        };
        match self.line {
            Some(number) => write!(f, "line {number}: {reason}"),
            None => f.write_str(&reason),
        }
    }
}

impl std::error::Error for AnchorsError {}

/// The anchors for `dirs`: sorted, each once, with keys assigned in that
/// order. This is how the packer derives a record, so the same set of
/// directories always gets the same keys.
pub fn assign<I, S>(dirs: I) -> Result<Vec<Anchor>, AnchorsError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut dirs: Vec<String> = dirs.into_iter().map(Into::into).collect();
    dirs.sort();
    dirs.dedup();
    if dirs.len() > MAX_ANCHORS {
        return Err(AnchorsError::record(AnchorsErrorKind::TooManyAnchors));
    }
    let anchors: Vec<Anchor> = dirs
        .into_iter()
        .enumerate()
        .map(|(index, relative)| Anchor {
            key: key(index),
            relative,
        })
        .collect();
    encode(&anchors)?;
    Ok(anchors)
}

/// Encode anchors as a record, refusing any record [`decode`] would refuse.
pub fn encode(anchors: &[Anchor]) -> Result<Vec<u8>, AnchorsError> {
    if anchors.len() > MAX_ANCHORS {
        return Err(AnchorsError::record(AnchorsErrorKind::TooManyAnchors));
    }
    let mut out = Vec::new();
    for (index, anchor) in anchors.iter().enumerate() {
        let previous = index.checked_sub(1).map(|i| anchors[i].relative.as_str());
        check_anchor(index, &anchor.key, &anchor.relative, previous)?;
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
        let previous = anchors.last().map(|a| a.relative.as_str());
        check_anchor(index, key, relative, previous)?;
        anchors.push(Anchor {
            key: key.to_owned(),
            relative: relative.to_owned(),
        });
    }
    Ok(anchors)
}

/// Check the anchor at `index`: its key is the one the position assigns,
/// its directory is one a record may name and sorts after `previous`.
fn check_anchor(
    index: usize,
    key_text: &str,
    relative: &str,
    previous: Option<&str>,
) -> Result<(), AnchorsError> {
    if key_text != key(index) {
        return Err(AnchorsError::line(AnchorsErrorKind::WrongKey, index));
    }
    if let Some(kind) = paths::relative_error(relative) {
        return Err(AnchorsError::line(AnchorsErrorKind::Directory(kind), index));
    }
    if previous.is_some_and(|previous| relative <= previous) {
        return Err(AnchorsError::line(AnchorsErrorKind::Unsorted, index));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(bytes: &[u8]) -> AnchorsErrorKind {
        decode(bytes).unwrap_err().kind()
    }

    #[test]
    fn keys_are_as_short_as_the_count_allows() {
        assert_eq!(key(0), "a");
        assert_eq!(key(25), "z");
        assert_eq!(key(26), "aa");
        assert_eq!(key(27), "ab");
        assert_eq!(key(26 + 26 * 26 - 1), "zz");
        assert_eq!(key(26 + 26 * 26), "aaa");
        assert!(key(MAX_ANCHORS - 1).len() <= 3);
        let all: std::collections::BTreeSet<String> = (0..MAX_ANCHORS).map(key).collect();
        assert_eq!(all.len(), MAX_ANCHORS, "no key repeats");
    }

    #[test]
    fn assignment_is_sorted_unique_and_round_trips() {
        let anchors = assign(["share/zz", "lib/alsa-lib", "share/alsa", "lib/alsa-lib"]).unwrap();
        let expected = [
            ("a", "lib/alsa-lib"),
            ("b", "share/alsa"),
            ("c", "share/zz"),
        ];
        let got: Vec<(&str, &str)> = anchors
            .iter()
            .map(|a| (a.key.as_str(), a.relative.as_str()))
            .collect();
        assert_eq!(got, expected);
        assert_eq!(anchors[1].path(), "/dev/fd/77/b");
        let bytes = encode(&anchors).unwrap();
        assert_eq!(bytes, b"a\tlib/alsa-lib\nb\tshare/alsa\nc\tshare/zz");
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
            (b"b\tshare/x", AnchorsErrorKind::WrongKey),
            (b"a\tshare/x\nc\tshare/y", AnchorsErrorKind::WrongKey),
            (b"A\tshare/x", AnchorsErrorKind::WrongKey),
            (b"a\tshare/y\nb\tshare/x", AnchorsErrorKind::Unsorted),
            (b"a\tshare/x\nb\tshare/x", AnchorsErrorKind::Unsorted),
            (b"\xff\tshare/x", AnchorsErrorKind::NotUtf8),
        ];
        for (bytes, kind) in cases {
            assert_eq!(refused(bytes), *kind, "{bytes:?}");
        }
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
                assign([relative]).unwrap_err().kind(),
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
        let dirs: Vec<String> = (0..=MAX_ANCHORS).map(|i| format!("d/{i:05}")).collect();
        assert_eq!(
            assign(dirs.clone()).unwrap_err().kind(),
            AnchorsErrorKind::TooManyAnchors
        );
        let lines: Vec<String> = dirs
            .iter()
            .enumerate()
            .map(|(i, d)| format!("{}\t{d}", key(i)))
            .collect();
        assert_eq!(
            refused(lines.join("\n").as_bytes()),
            AnchorsErrorKind::TooManyAnchors
        );
        assert!(assign(dirs[..MAX_ANCHORS].to_vec()).is_ok());
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
