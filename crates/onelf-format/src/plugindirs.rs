//! The plugin directory record, `.onelf/plugin-dirs`.
//!
//! A package carries plugin directories, whether a descriptor's group
//! carries them or inference does. A bundled library usually compiles in
//! the absolute path of the directory it was built beside, and the host
//! keeps its own copy of the same directory, possibly under a different
//! library directory on a multiarch distribution. The runtime reads this
//! record to know which directories those are, so it can consider the
//! host's modules in their counterparts.
//!
//! Each line names one carried directory, as
//! `RELATIVE<tab>LIB_RELATIVE[<tab>ABSOLUTE]*`. The relative side is the
//! directory's package path. The library side is the directory's path
//! under a host library directory, such as `alsa-lib` for
//! `/usr/lib/alsa-lib`, and is empty when the package's layout names no
//! such counterpart. Each absolute side is a path an object was built to
//! open the directory by; a described group's is usually unknown and the
//! field is then absent.
//!
//! Lines are separated by a single `\n` and the record carries no
//! trailing newline, one directory per line. The packer writes them
//! sorted by the relative side, each once.
//!
//! The record is parsed strictly and within named bounds, like the
//! anchors and paths records. Every named path is normalized: no `.` or
//! `..` component, no empty component and no control character (C0, DEL
//! or C1), NUL included. A relative side stays inside the package, a
//! library side stays relative, and an absolute side names a directory
//! below the root. Anything else is refused as a whole, because a record
//! the packer would not have written is damage rather than an
//! instruction.
//!
//! [`encode`] applies the same rules as [`decode`], so the packer
//! refuses at build time every record a runtime would refuse at launch,
//! and every accepted record re-encodes to the same bytes.

use std::collections::BTreeSet;
use std::fmt;

use crate::paths::{MAX_RELATIVE_BYTES, PathsErrorKind, absolute_error, excerpt, relative_error};

/// Where a package carries the record.
pub const PLUGIN_DIRS_FILE: &str = ".onelf/plugin-dirs";
/// The most directories one record holds.
pub const MAX_DIRS: usize = 256;
/// The largest complete record, in bytes, checked before anything is
/// parsed or allocated.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;

/// One carried plugin directory: its package path, its path under a
/// host library directory when one is known, and every compiled-in
/// path an object names it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDir {
    /// The directory's package path, relative to the package root.
    pub relative: String,
    /// The directory's path under a host library directory, such as
    /// `alsa-lib` for `/usr/lib/alsa-lib`, or empty when the package's
    /// layout names no such counterpart.
    pub lib_relative: String,
    /// Every compiled-in path an object opens the directory by.
    pub absolutes: Vec<String>,
}

/// Why a record or a directory was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginDirsErrorKind {
    /// The record is over [`MAX_RECORD_BYTES`].
    RecordTooLarge,
    /// The record holds more than [`MAX_DIRS`] directories.
    TooManyDirs,
    /// The record is not UTF-8.
    NotUtf8,
    /// The line holds no directory: a blank line, or a trailing newline.
    EmptyLine,
    /// The line has no tab after its package path.
    MissingSeparator,
    /// The package path was already named earlier in the record.
    DuplicateDir,
    /// An absolute path was already named earlier in the record.
    DuplicateTarget,
    /// The package path is not one a record may name.
    Relative(PathsErrorKind),
    /// The library side is not one a record may name.
    LibRelative(PathsErrorKind),
    /// An absolute path is not one a record may name.
    Absolute(PathsErrorKind),
}

/// A refused record or directory, naming its line, numbered from 1, and
/// the path that caused it when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDirsError {
    kind: PluginDirsErrorKind,
    line: Option<usize>,
    path: Option<String>,
}

impl PluginDirsError {
    fn record(kind: PluginDirsErrorKind) -> Self {
        Self {
            kind,
            line: None,
            path: None,
        }
    }

    fn line(kind: PluginDirsErrorKind, index: usize) -> Self {
        Self {
            kind,
            line: Some(index + 1),
            path: None,
        }
    }

    fn dir(kind: PluginDirsErrorKind, index: usize, path: &str) -> Self {
        Self {
            kind,
            line: Some(index + 1),
            path: Some(excerpt(path)),
        }
    }

    /// Why the record was refused.
    pub fn kind(&self) -> PluginDirsErrorKind {
        self.kind
    }
}

impl PluginDirsErrorKind {
    /// The refusal in words, without where it lies.
    fn reason(self) -> String {
        match self {
            Self::RecordTooLarge => {
                format!("the record is over {MAX_RECORD_BYTES} bytes")
            }
            Self::TooManyDirs => format!("the record holds over {MAX_DIRS} directories"),
            Self::NotUtf8 => "the record is not UTF-8".into(),
            Self::EmptyLine => "the line holds no directory".into(),
            Self::MissingSeparator => "the line has no tab after its package path".into(),
            Self::DuplicateDir => "the directory is named more than once".into(),
            Self::DuplicateTarget => "the absolute path is named more than once".into(),
            Self::Relative(kind) => kind.reason(),
            Self::Absolute(kind) => kind.reason(),
            Self::LibRelative(kind) => match kind {
                PathsErrorKind::RelativeTooLong => {
                    format!("the library path is over {MAX_RELATIVE_BYTES} bytes")
                }
                PathsErrorKind::RelativeIsAbsolute => {
                    "the library path must be relative to a library directory".into()
                }
                PathsErrorKind::Escapes => "the library path leaves its library directory".into(),
                other => other.reason(),
            },
        }
    }
}

impl fmt::Display for PluginDirsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = self.kind.reason();
        match (&self.line, &self.path) {
            (Some(number), Some(path)) => write!(f, "line {number}, \"{path}\": {reason}"),
            (Some(number), None) => write!(f, "line {number}: {reason}"),
            _ => f.write_str(&reason),
        }
    }
}

impl std::error::Error for PluginDirsError {}

/// Encode directories as a record, refusing any record [`decode`]
/// would refuse.
///
/// Each directory's position in `dirs` is the line it lands on, so a
/// refusal names it. The record's size is counted before any line is
/// validated or written, so nothing large is built on the way to a
/// refusal.
pub fn encode(dirs: &[PluginDir]) -> Result<Vec<u8>, PluginDirsError> {
    if dirs.len() > MAX_DIRS {
        return Err(PluginDirsError::record(PluginDirsErrorKind::TooManyDirs));
    }
    let mut relatives = BTreeSet::new();
    let mut targets = BTreeSet::new();
    let mut out = Vec::new();
    for (index, dir) in dirs.iter().enumerate() {
        let mut needed = usize::from(index > 0) + dir.relative.len() + 1 + dir.lib_relative.len();
        for absolute in &dir.absolutes {
            needed = needed.saturating_add(1 + absolute.len());
        }
        if out.len().saturating_add(needed) > MAX_RECORD_BYTES {
            return Err(PluginDirsError::record(PluginDirsErrorKind::RecordTooLarge));
        }
        check_dir(
            index,
            &dir.relative,
            &dir.lib_relative,
            dir.absolutes.iter().map(String::as_str),
            &mut relatives,
            &mut targets,
        )?;
        if index > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(dir.relative.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(dir.lib_relative.as_bytes());
        for absolute in &dir.absolutes {
            out.push(b'\t');
            out.extend_from_slice(absolute.as_bytes());
        }
    }
    Ok(out)
}

/// Decode a record into its directories, in record order.
///
/// The size is checked before the bytes are looked at, so nothing
/// allocated here is larger than [`MAX_RECORD_BYTES`] allows.
pub fn decode(bytes: &[u8]) -> Result<Vec<PluginDir>, PluginDirsError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(PluginDirsError::record(PluginDirsErrorKind::RecordTooLarge));
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| PluginDirsError::record(PluginDirsErrorKind::NotUtf8))?;
    let mut dirs = Vec::new();
    let mut relatives = BTreeSet::new();
    let mut targets = BTreeSet::new();
    for (index, line) in text.split('\n').enumerate() {
        if index >= MAX_DIRS {
            return Err(PluginDirsError::record(PluginDirsErrorKind::TooManyDirs));
        }
        if line.is_empty() {
            return Err(PluginDirsError::line(PluginDirsErrorKind::EmptyLine, index));
        }
        let Some((relative, rest)) = line.split_once('\t') else {
            return Err(PluginDirsError::line(
                PluginDirsErrorKind::MissingSeparator,
                index,
            ));
        };
        let mut fields = rest.split('\t');
        let lib_relative = fields.next().unwrap_or_default();
        let absolutes: Vec<&str> = fields.collect();
        check_dir(
            index,
            relative,
            lib_relative,
            absolutes.iter().copied(),
            &mut relatives,
            &mut targets,
        )?;
        dirs.push(PluginDir {
            relative: relative.to_owned(),
            lib_relative: lib_relative.to_owned(),
            absolutes: absolutes.iter().map(|a| (*a).to_owned()).collect(),
        });
    }
    Ok(dirs)
}

/// Check the directory at `index`: each side is one a record may name,
/// its package path is new to `relatives` and its absolute paths are new
/// to `targets`. An empty library side names no counterpart and is
/// allowed.
fn check_dir<'a>(
    index: usize,
    relative: &'a str,
    lib_relative: &str,
    absolutes: impl IntoIterator<Item = &'a str>,
    relatives: &mut BTreeSet<&'a str>,
    targets: &mut BTreeSet<&'a str>,
) -> Result<(), PluginDirsError> {
    let refuse = |kind, path: &str| Err(PluginDirsError::dir(kind, index, path));
    if let Some(kind) = relative_error(relative) {
        return refuse(PluginDirsErrorKind::Relative(kind), relative);
    }
    if !relatives.insert(relative) {
        return refuse(PluginDirsErrorKind::DuplicateDir, relative);
    }
    if !lib_relative.is_empty()
        && let Some(kind) = relative_error(lib_relative)
    {
        return refuse(PluginDirsErrorKind::LibRelative(kind), lib_relative);
    }
    for absolute in absolutes {
        if let Some(kind) = absolute_error(absolute) {
            return refuse(PluginDirsErrorKind::Absolute(kind), absolute);
        }
        if !targets.insert(absolute) {
            return refuse(PluginDirsErrorKind::DuplicateTarget, absolute);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{EXCERPT_CHARS, MAX_ABSOLUTE_BYTES};

    fn refused(bytes: &[u8]) -> PluginDirsErrorKind {
        decode(bytes).unwrap_err().kind()
    }

    fn refused_dir(dir: PluginDir) -> PluginDirsErrorKind {
        encode(&[dir]).unwrap_err().kind()
    }

    /// Why one directory is refused, if it is not: the same kind through
    /// both APIs, so the packer can never write a line decode refuses.
    fn refused_sides(
        relative: &str,
        lib_relative: &str,
        absolutes: &[&str],
    ) -> PluginDirsErrorKind {
        let mut line = format!("{relative}\t{lib_relative}");
        for absolute in absolutes {
            line.push('\t');
            line.push_str(absolute);
        }
        let from_decode = refused(line.as_bytes());
        let from_encode = refused_dir(dir(relative, lib_relative, absolutes));
        assert_eq!(from_decode, from_encode);
        from_decode
    }

    fn dir(relative: &str, lib_relative: &str, absolutes: &[&str]) -> PluginDir {
        PluginDir {
            relative: relative.to_owned(),
            lib_relative: lib_relative.to_owned(),
            absolutes: absolutes.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    #[test]
    fn an_empty_record_holds_no_directories() {
        assert_eq!(decode(b"").unwrap(), Vec::new());
        assert_eq!(encode(&[]).unwrap(), Vec::new());
    }

    #[test]
    fn a_line_carries_its_sides() {
        let dirs = decode(b"lib/alsa-lib\talsa-lib\t/usr/lib/alsa-lib\t/lib/alsa-lib").unwrap();
        assert_eq!(
            dirs,
            vec![dir(
                "lib/alsa-lib",
                "alsa-lib",
                &["/usr/lib/alsa-lib", "/lib/alsa-lib"]
            )]
        );
        let record = encode(&dirs).unwrap();
        assert_eq!(
            record,
            b"lib/alsa-lib\talsa-lib\t/usr/lib/alsa-lib\t/lib/alsa-lib"
        );
        assert_eq!(decode(&record).unwrap(), dirs);
    }

    #[test]
    fn an_unknown_library_side_is_allowed() {
        let dirs = decode(b"lib/xorg/modules\t").unwrap();
        assert_eq!(dirs, vec![dir("lib/xorg/modules", "", &[])]);
        assert_eq!(encode(&dirs).unwrap(), b"lib/xorg/modules\t");
    }

    #[test]
    fn a_record_without_a_library_side_is_still_strict() {
        assert_eq!(
            refused(b"lib/xorg/modules"),
            PluginDirsErrorKind::MissingSeparator
        );
    }

    #[test]
    fn directories_stay_in_record_order() {
        let dirs = decode(b"b\tb\na\ta\t/a").unwrap();
        assert_eq!(dirs, vec![dir("b", "b", &[]), dir("a", "a", &["/a"])]);
    }

    #[test]
    fn the_same_bytes_encode_always() {
        let dirs = vec![
            dir("a", "a", &["/a"]),
            dir("b", "", &[]),
            dir("c/d", "d", &["/c/d"]),
        ];
        let first = encode(&dirs).unwrap();
        let second = encode(&dirs).unwrap();
        assert_eq!(first, second);
        assert_eq!(decode(&first).unwrap(), dirs);
    }

    #[test]
    fn lines_hold_no_blanks() {
        assert_eq!(refused(b"a\tb\n"), PluginDirsErrorKind::EmptyLine);
        assert_eq!(refused(b"a\tb\n\nc\td"), PluginDirsErrorKind::EmptyLine);
        assert_eq!(refused(b"\na\tb"), PluginDirsErrorKind::EmptyLine);
    }

    #[test]
    fn a_line_without_a_tab_is_refused() {
        assert_eq!(refused(b"alsa-lib"), PluginDirsErrorKind::MissingSeparator);
    }

    #[test]
    fn a_record_must_be_utf8() {
        assert_eq!(refused(b"a\t\xffb"), PluginDirsErrorKind::NotUtf8);
    }

    #[test]
    fn a_directory_is_named_once() {
        assert_eq!(
            refused(b"a\tb\nc\td\na\tb"),
            PluginDirsErrorKind::DuplicateDir
        );
        assert_eq!(
            encode(&[dir("a", "b", &[]), dir("a", "c", &[])])
                .unwrap_err()
                .kind(),
            PluginDirsErrorKind::DuplicateDir
        );
    }

    #[test]
    fn an_absolute_path_is_named_once() {
        assert_eq!(
            refused(b"a\tb\t/x\nc\td\t/x"),
            PluginDirsErrorKind::DuplicateTarget
        );
        assert_eq!(
            refused_dir(dir("a", "b", &["/x", "/x"])),
            PluginDirsErrorKind::DuplicateTarget
        );
    }

    #[test]
    fn the_package_path_follows_the_bundle_rules() {
        assert_eq!(
            refused_sides("/usr/lib/alsa-lib", "alsa-lib", &[]),
            PluginDirsErrorKind::Relative(PathsErrorKind::RelativeIsAbsolute)
        );
        assert_eq!(
            refused_sides("../alsa-lib", "alsa-lib", &[]),
            PluginDirsErrorKind::Relative(PathsErrorKind::Escapes)
        );
        assert_eq!(
            refused_sides("alsa-lib/", "alsa-lib", &[]),
            PluginDirsErrorKind::Relative(PathsErrorKind::Unnormalized)
        );
        assert_eq!(
            refused_sides("alsa//lib", "alsa-lib", &[]),
            PluginDirsErrorKind::Relative(PathsErrorKind::Unnormalized)
        );
        assert_eq!(
            refused_sides("alsa/\x01lib", "alsa-lib", &[]),
            PluginDirsErrorKind::Relative(PathsErrorKind::ControlCharacter)
        );
        let long = "a".repeat(MAX_RELATIVE_BYTES + 1);
        assert_eq!(
            refused_sides(&long, "alsa-lib", &[]),
            PluginDirsErrorKind::Relative(PathsErrorKind::RelativeTooLong)
        );
    }

    #[test]
    fn the_library_side_stays_under_a_library_directory() {
        assert_eq!(
            refused_sides("a", "/alsa-lib", &[]),
            PluginDirsErrorKind::LibRelative(PathsErrorKind::RelativeIsAbsolute)
        );
        assert_eq!(
            refused_sides("a", "alsa\x01lib", &[]),
            PluginDirsErrorKind::LibRelative(PathsErrorKind::ControlCharacter)
        );
        assert_eq!(
            refused_sides("a", "../alsa-lib", &[]),
            PluginDirsErrorKind::LibRelative(PathsErrorKind::Escapes)
        );
        assert_eq!(
            refused_sides("a", ".", &[]),
            PluginDirsErrorKind::LibRelative(PathsErrorKind::Unnormalized)
        );
        let long = "a".repeat(MAX_RELATIVE_BYTES + 1);
        assert_eq!(
            refused_sides("a", &long, &[]),
            PluginDirsErrorKind::LibRelative(PathsErrorKind::RelativeTooLong)
        );
    }

    #[test]
    fn an_absolute_side_names_a_normalized_directory() {
        assert_eq!(
            refused_sides("a", "b", &["c"]),
            PluginDirsErrorKind::Absolute(PathsErrorKind::NotAbsolute)
        );
        assert_eq!(
            refused_sides("a", "b", &["/usr/lib//alsa-lib"]),
            PluginDirsErrorKind::Absolute(PathsErrorKind::Unnormalized)
        );
        assert_eq!(
            refused_sides("a", "b", &["/usr/lib/./alsa-lib"]),
            PluginDirsErrorKind::Absolute(PathsErrorKind::Unnormalized)
        );
        assert_eq!(
            refused_sides("a", "b", &["/usr/lib/alsa-lib/../x"]),
            PluginDirsErrorKind::Absolute(PathsErrorKind::Unnormalized)
        );
        assert_eq!(
            refused_sides("a", "b", &["/usr/lib/alsa\x01lib"]),
            PluginDirsErrorKind::Absolute(PathsErrorKind::ControlCharacter)
        );
        let long = format!("/{}", "a".repeat(MAX_ABSOLUTE_BYTES));
        assert_eq!(
            refused_sides("a", "b", &[&long]),
            PluginDirsErrorKind::Absolute(PathsErrorKind::AbsoluteTooLong)
        );
        let exact = format!("/{}", "a".repeat(MAX_ABSOLUTE_BYTES - 1));
        assert_eq!(
            encode(&[dir("a", "b", &[&exact])]).unwrap(),
            format!("a\tb\t{exact}").into_bytes()
        );
    }

    #[test]
    fn a_record_holds_at_most_max_dirs_directories() {
        let dirs: Vec<PluginDir> = (0..MAX_DIRS)
            .map(|i| dir(&format!("d{i}"), "x", &[]))
            .collect();
        let record = encode(&dirs).unwrap();
        assert_eq!(decode(&record).unwrap().len(), MAX_DIRS);
        let dirs: Vec<PluginDir> = (0..MAX_DIRS + 1)
            .map(|i| dir(&format!("d{i}"), "x", &[]))
            .collect();
        assert_eq!(
            encode(&dirs).unwrap_err().kind(),
            PluginDirsErrorKind::TooManyDirs
        );
        let lines: Vec<String> = (0..MAX_DIRS + 1).map(|i| format!("d{i}\tx")).collect();
        let record = lines.join("\n");
        assert_eq!(refused(record.as_bytes()), PluginDirsErrorKind::TooManyDirs);
    }

    #[test]
    fn a_record_holds_at_most_max_record_bytes() {
        let dirs: Vec<PluginDir> = (0..64)
            .map(|i| dir(&format!("{}/{}", "a".repeat(1024), i), "x", &[]))
            .collect();
        assert_eq!(
            encode(&dirs).unwrap_err().kind(),
            PluginDirsErrorKind::RecordTooLarge
        );
        let dirs: Vec<PluginDir> = (0..63)
            .map(|i| dir(&format!("{}/{}", "a".repeat(1024), i), "x", &[]))
            .collect();
        let record = encode(&dirs).unwrap();
        assert!(record.len() <= MAX_RECORD_BYTES);
        assert_eq!(decode(&record).unwrap().len(), 63);
    }

    #[test]
    fn a_record_of_exactly_the_size_bound_round_trips() {
        let chars: Vec<char> = "abcdefghijklmnop".chars().collect();
        let dirs: Vec<PluginDir> = (0..16)
            .map(|i| {
                let lib = "a".repeat(2046 + usize::from(i == 15));
                dir(&format!("{}{}", "a".repeat(2047), chars[i]), &lib, &[])
            })
            .collect();
        let record = encode(&dirs).unwrap();
        assert_eq!(record.len(), MAX_RECORD_BYTES);
        assert_eq!(decode(&record).unwrap(), dirs);
        assert_eq!(encode(&decode(&record).unwrap()).unwrap(), record);
    }

    #[test]
    fn a_record_one_byte_over_the_size_bound_is_refused() {
        let chars: Vec<char> = "abcdefghijklmnop".chars().collect();
        let mut dirs: Vec<PluginDir> = (0..16)
            .map(|i| {
                let lib = "a".repeat(2046 + usize::from(i == 15));
                dir(&format!("{}{}", "a".repeat(2047), chars[i]), &lib, &[])
            })
            .collect();
        dirs[15].lib_relative = "a".repeat(2048);
        assert_eq!(
            encode(&dirs).unwrap_err().kind(),
            PluginDirsErrorKind::RecordTooLarge
        );
    }

    #[test]
    fn an_oversized_record_is_refused_before_it_is_read() {
        assert_eq!(
            refused(&vec![b'a'; MAX_RECORD_BYTES + 1]),
            PluginDirsErrorKind::RecordTooLarge
        );
        assert_eq!(
            refused(&vec![0xff; MAX_RECORD_BYTES + 1]),
            PluginDirsErrorKind::RecordTooLarge
        );
    }

    #[test]
    fn one_directory_cannot_exceed_the_record_bound() {
        let absolutes: Vec<String> = (0..80)
            .map(|i| format!("/lib/{i}/{}", "a".repeat(1024)))
            .collect();
        let refs: Vec<&str> = absolutes.iter().map(String::as_str).collect();
        assert_eq!(
            encode(&[dir("a", "b", &refs)]).unwrap_err().kind(),
            PluginDirsErrorKind::RecordTooLarge
        );
    }

    #[test]
    fn a_refusal_names_its_line_and_path() {
        let error = encode(&[dir("a", "b", &[]), dir("c", "", &["bad"])]).unwrap_err();
        assert_eq!(
            error.kind(),
            PluginDirsErrorKind::Absolute(PathsErrorKind::NotAbsolute)
        );
        let text = error.to_string();
        assert!(text.starts_with("line 2, \"bad\": "), "{text}");
        let error = decode(b"a\tb\nc\td\na\tb").unwrap_err();
        assert!(
            error.to_string().starts_with("line 3, \"a\": "),
            "{}",
            error
        );
    }

    #[test]
    fn a_refusal_excerpts_a_long_path() {
        let long = format!("/{}", "a".repeat(MAX_ABSOLUTE_BYTES));
        let error = encode(&[dir("a", "b", &[&long])]).unwrap_err();
        assert_eq!(
            error.kind(),
            PluginDirsErrorKind::Absolute(PathsErrorKind::AbsoluteTooLong)
        );
        let text = error.to_string();
        assert!(text.contains(&long[..EXCERPT_CHARS - 8]), "{text}");
        assert!(!text.contains(&long[..EXCERPT_CHARS + 8]), "{text}");
    }
}
