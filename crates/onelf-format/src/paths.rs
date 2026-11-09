//! The path overlay record, `.onelf/paths`.
//!
//! Each line pairs an absolute path an application was built to open
//! with the bundle directory that answers for it, as
//! `ABSOLUTE<tab>RELATIVE`. Lines are separated by a single `\n` and the
//! record carries no trailing newline, which is the form the packer has
//! always written, so a recipe's pairs keep their exact bytes.
//!
//! The runtime binds every pair in a private mount namespace on an end
//! user's machine, so the record is parsed strictly and within named
//! bounds. Both sides must be normalized: no `.` or `..` component, no
//! empty component (and so no doubled or trailing `/`), and no control
//! character (C0, DEL or C1), NUL included. The absolute side names a
//! directory below the root, and the bundle side is relative and stays
//! inside the package. That containment is lexical: a symlink inside the
//! package is followed where the bundle side names it, as it is for every
//! package path. An absolute target may appear once. Anything else, from
//! a blank line to a third field, is refused as a whole, because a record
//! the packer would not have written is damage rather than an
//! instruction.
//!
//! [`encode`] applies the same rules as [`decode`], so the packer refuses
//! at build time every pair a runtime would refuse at launch, and every
//! accepted record re-encodes to the same bytes. Pair order is kept.

use std::collections::BTreeSet;
use std::fmt;

/// Where a package carries the record.
pub const PATHS_FILE: &str = ".onelf/paths";
/// The most pairs one record holds.
pub const MAX_PAIRS: usize = 1024;
/// The longest absolute side, in bytes. The kernel's `PATH_MAX` of 4096
/// counts the terminating NUL.
pub const MAX_ABSOLUTE_BYTES: usize = 4095;
/// The longest bundle side, in bytes. It is joined to the package root
/// at launch, so it leaves room for that root within `PATH_MAX`.
pub const MAX_RELATIVE_BYTES: usize = 2048;
/// The largest complete record, in bytes, checked before anything is
/// parsed or allocated.
pub const MAX_RECORD_BYTES: usize = 256 * 1024;

/// The longest excerpt of a path an error quotes, in characters.
pub(crate) const EXCERPT_CHARS: usize = 128;

/// One overlay: the absolute path and the bundle directory, relative to
/// the package root, that answers for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPair {
    /// The absolute path the application opens.
    pub absolute: String,
    /// The bundle directory, relative to the package root.
    pub relative: String,
}

/// Why a record or a pair was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathsErrorKind {
    /// The record is over [`MAX_RECORD_BYTES`].
    RecordTooLarge,
    /// The record holds more than [`MAX_PAIRS`] pairs.
    TooManyPairs,
    /// The record is not UTF-8.
    NotUtf8,
    /// A line holds no pair: a blank line, or a trailing newline.
    EmptyLine,
    /// A line has no tab between its sides.
    MissingSeparator,
    /// A line has a tab after its bundle side.
    TrailingField,
    /// The absolute side does not start with `/`.
    NotAbsolute,
    /// The absolute side is over [`MAX_ABSOLUTE_BYTES`].
    AbsoluteTooLong,
    /// The bundle side starts with `/`.
    RelativeIsAbsolute,
    /// The bundle side is over [`MAX_RELATIVE_BYTES`].
    RelativeTooLong,
    /// The bundle side has a `..` component, which leaves the package.
    Escapes,
    /// A side has an empty or `.` component, or names its own base, or
    /// the absolute side has a `..` component.
    Unnormalized,
    /// A side holds a control character: C0, DEL or C1, NUL included.
    ControlCharacter,
    /// The absolute side was already paired earlier in the record.
    DuplicateTarget,
}

/// A refused record or pair, naming the line or pair when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathsError {
    kind: PathsErrorKind,
    at: Locus,
}

/// Where an error lies. A decoded record names the line, numbered from
/// 1, and an encoded pair is named by its quoted sides alone, since the
/// caller's order need not be the order its author wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Locus {
    Record,
    Line(usize),
    LinePair(usize, String),
    Pair(String),
}

impl PathsError {
    fn record(kind: PathsErrorKind) -> Self {
        Self {
            kind,
            at: Locus::Record,
        }
    }

    fn line(kind: PathsErrorKind, index: usize) -> Self {
        Self {
            kind,
            at: Locus::Line(index + 1),
        }
    }

    fn pair(kind: PathsErrorKind, line: Option<usize>, absolute: &str, relative: &str) -> Self {
        let quoted = format!("\"{}\" = \"{}\"", excerpt(absolute), excerpt(relative));
        let at = match line {
            Some(index) => Locus::LinePair(index + 1, quoted),
            None => Locus::Pair(quoted),
        };
        Self { kind, at }
    }

    /// Why the record was refused.
    pub fn kind(&self) -> PathsErrorKind {
        self.kind
    }
}

impl PathsErrorKind {
    /// The refusal in words, without where it lies.
    pub(crate) fn reason(self) -> String {
        match self {
            Self::RecordTooLarge => {
                format!("the record is over {MAX_RECORD_BYTES} bytes")
            }
            Self::TooManyPairs => format!("the record holds over {MAX_PAIRS} pairs"),
            Self::NotUtf8 => "the record is not UTF-8".into(),
            Self::EmptyLine => "the line holds no pair".into(),
            Self::MissingSeparator => "the line has no tab between its paths".into(),
            Self::TrailingField => "the line goes on after the bundle path".into(),
            Self::NotAbsolute => "the target is not an absolute path".into(),
            Self::AbsoluteTooLong => {
                format!("the target is over {MAX_ABSOLUTE_BYTES} bytes")
            }
            Self::RelativeIsAbsolute => {
                "the bundle path must be relative to the package root".into()
            }
            Self::RelativeTooLong => {
                format!("the bundle path is over {MAX_RELATIVE_BYTES} bytes")
            }
            Self::Escapes => "the bundle path leaves the package".into(),
            Self::Unnormalized => {
                "a path is not normalized, or names no directory below its base".into()
            }
            Self::ControlCharacter => "a path holds a control character".into(),
            Self::DuplicateTarget => "the target is paired more than once".into(),
        }
    }
}

impl fmt::Display for PathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = self.kind.reason();
        match &self.at {
            Locus::Record => f.write_str(&reason),
            Locus::Line(number) => write!(f, "line {number}: {reason}"),
            Locus::LinePair(number, quoted) => write!(f, "line {number}, {quoted}: {reason}"),
            Locus::Pair(quoted) => write!(f, "{quoted}: {reason}"),
        }
    }
}

impl std::error::Error for PathsError {}

/// Decode a record into its pairs, in record order.
///
/// The size is checked before the bytes are looked at, so nothing
/// allocated here is larger than [`MAX_RECORD_BYTES`] allows.
pub fn decode(bytes: &[u8]) -> Result<Vec<PathPair>, PathsError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(PathsError::record(PathsErrorKind::RecordTooLarge));
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| PathsError::record(PathsErrorKind::NotUtf8))?;
    let mut pairs = Vec::new();
    let mut targets = BTreeSet::new();
    for (index, line) in text.split('\n').enumerate() {
        if index >= MAX_PAIRS {
            return Err(PathsError::record(PathsErrorKind::TooManyPairs));
        }
        if line.is_empty() {
            return Err(PathsError::line(PathsErrorKind::EmptyLine, index));
        }
        let Some((absolute, relative)) = line.split_once('\t') else {
            return Err(PathsError::line(PathsErrorKind::MissingSeparator, index));
        };
        if relative.contains('\t') {
            return Err(PathsError::pair(
                PathsErrorKind::TrailingField,
                Some(index),
                absolute,
                relative,
            ));
        }
        check_pair(Some(index), absolute, relative, &mut targets)?;
        pairs.push(PathPair {
            absolute: absolute.to_owned(),
            relative: relative.to_owned(),
        });
    }
    Ok(pairs)
}

/// Encode pairs as a record, refusing any pair [`decode`] would refuse.
pub fn encode(pairs: &[PathPair]) -> Result<Vec<u8>, PathsError> {
    if pairs.len() > MAX_PAIRS {
        return Err(PathsError::record(PathsErrorKind::TooManyPairs));
    }
    let mut targets = BTreeSet::new();
    let mut out = Vec::new();
    for (index, pair) in pairs.iter().enumerate() {
        check_pair(None, &pair.absolute, &pair.relative, &mut targets)?;
        if index > 0 {
            out.push(b'\n');
        }
        out.extend_from_slice(pair.absolute.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(pair.relative.as_bytes());
        if out.len() > MAX_RECORD_BYTES {
            return Err(PathsError::record(PathsErrorKind::RecordTooLarge));
        }
    }
    Ok(out)
}

/// Check one pair's sides, and that its target is new to `targets`.
/// `line` is the pair's record line index, when it was decoded from one.
fn check_pair<'a>(
    line: Option<usize>,
    absolute: &'a str,
    relative: &str,
    targets: &mut BTreeSet<&'a str>,
) -> Result<(), PathsError> {
    let refuse = |kind| Err(PathsError::pair(kind, line, absolute, relative));
    if let Some(kind) = absolute_error(absolute) {
        return refuse(kind);
    }
    if let Some(kind) = relative_error(relative) {
        return refuse(kind);
    }
    if !targets.insert(absolute) {
        return refuse(PathsErrorKind::DuplicateTarget);
    }
    Ok(())
}

/// Why a pair the packer would write is refused, as one sentence
/// quoting both sides, if it is refused: the same checks one record
/// line goes through, without the line. The packer's pair extraction
/// skips a pair this refuses, so a build carries no pair its runtime
/// ignores.
pub fn pair_refusal(absolute: &str, relative: &str) -> Option<String> {
    check_pair(None, absolute, relative, &mut BTreeSet::new())
        .err()
        .map(|error| error.to_string())
}

/// Why `relative` is not a bundle directory a record may name, if it is
/// not: over [`MAX_RELATIVE_BYTES`], holding a control character,
/// absolute, leaving the package through `..`, or not normalized. Shared
/// with the other records that name bundle directories, so each refuses
/// exactly the same paths.
pub(crate) fn relative_error(relative: &str) -> Option<PathsErrorKind> {
    if relative.len() > MAX_RELATIVE_BYTES {
        return Some(PathsErrorKind::RelativeTooLong);
    }
    if relative.chars().any(char::is_control) {
        return Some(PathsErrorKind::ControlCharacter);
    }
    if relative.starts_with('/') {
        return Some(PathsErrorKind::RelativeIsAbsolute);
    }
    if relative.split('/').any(|part| part == "..") {
        return Some(PathsErrorKind::Escapes);
    }
    if relative.split('/').any(|part| matches!(part, "" | ".")) {
        return Some(PathsErrorKind::Unnormalized);
    }
    None
}

/// Why `absolute` is not a target a record may name, if it is not: over
/// [`MAX_ABSOLUTE_BYTES`], holding a control character, not absolute, or
/// not normalized. Shared with the other records that name absolute
/// paths, so each refuses exactly the same paths.
pub(crate) fn absolute_error(absolute: &str) -> Option<PathsErrorKind> {
    if absolute.len() > MAX_ABSOLUTE_BYTES {
        return Some(PathsErrorKind::AbsoluteTooLong);
    }
    if absolute.chars().any(char::is_control) {
        return Some(PathsErrorKind::ControlCharacter);
    }
    let Some(below_root) = absolute.strip_prefix('/') else {
        return Some(PathsErrorKind::NotAbsolute);
    };
    if below_root
        .split('/')
        .any(|part| matches!(part, "" | "." | ".."))
    {
        return Some(PathsErrorKind::Unnormalized);
    }
    None
}

/// A bounded, escaped excerpt of a path for a diagnostic.
pub(crate) fn excerpt(path: &str) -> String {
    path.chars()
        .take(EXCERPT_CHARS)
        .flat_map(char::escape_default)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(absolute: &str, relative: &str) -> PathPair {
        PathPair {
            absolute: absolute.into(),
            relative: relative.into(),
        }
    }

    fn refused(bytes: &[u8]) -> PathsErrorKind {
        decode(bytes).unwrap_err().kind()
    }

    fn refused_pair(absolute: &str, relative: &str) -> PathsErrorKind {
        let encoded = encode(&[pair(absolute, relative)]).unwrap_err().kind();
        let line = format!("{absolute}\t{relative}");
        assert_eq!(refused(line.as_bytes()), encoded, "{line:?}");
        encoded
    }

    #[test]
    fn a_record_round_trips_byte_for_byte() {
        let bytes = b"/usr/share/galculator\tshare/galculator\n\
                      /opt/My Vendor/data\tshare/vendor data\n\
                      /usr/share/fixture-again\tshare/galculator";
        let pairs = decode(bytes).unwrap();
        assert_eq!(
            pairs,
            vec![
                pair("/usr/share/galculator", "share/galculator"),
                pair("/opt/My Vendor/data", "share/vendor data"),
                pair("/usr/share/fixture-again", "share/galculator"),
            ]
        );
        assert_eq!(encode(&pairs).unwrap(), bytes);
    }

    #[test]
    fn the_empty_record_holds_no_pairs() {
        assert_eq!(decode(b"").unwrap(), Vec::new());
        assert_eq!(encode(&[]).unwrap(), b"");
    }

    #[test]
    fn a_record_the_packer_would_not_write_is_refused() {
        assert_eq!(refused(b"/a\tb\n"), PathsErrorKind::EmptyLine);
        assert_eq!(refused(b"/a\tb\n\n/c\td"), PathsErrorKind::EmptyLine);
        assert_eq!(refused(b"\n/a\tb"), PathsErrorKind::EmptyLine);
        assert_eq!(refused(b"/a b"), PathsErrorKind::MissingSeparator);
        assert_eq!(refused(b"/a\tb\tc"), PathsErrorKind::TrailingField);
        assert_eq!(refused(b"/a\tb\t"), PathsErrorKind::TrailingField);
        assert_eq!(refused(b"/a\tb\r"), PathsErrorKind::ControlCharacter);
        assert_eq!(refused(b"/a\t\xffb"), PathsErrorKind::NotUtf8);
    }

    #[test]
    fn the_target_must_be_absolute_and_normalized() {
        for target in ["usr/share/x", " /usr/share/x", ""] {
            assert_eq!(refused_pair(target, "share/x"), PathsErrorKind::NotAbsolute);
        }
        for target in [
            "/",
            "//usr",
            "/usr//share",
            "/usr/share/",
            "/usr/./share",
            "/usr/share/.",
            "/usr/../etc",
            "/..",
        ] {
            assert_eq!(
                refused_pair(target, "share/x"),
                PathsErrorKind::Unnormalized,
                "{target}"
            );
        }
    }

    #[test]
    fn the_bundle_side_must_stay_inside_the_package() {
        assert_eq!(
            refused_pair("/usr/share/x", "/usr/share/x"),
            PathsErrorKind::RelativeIsAbsolute
        );
        for rel in ["..", "../x", "share/../../x", "share/.."] {
            assert_eq!(refused_pair("/usr/share/x", rel), PathsErrorKind::Escapes);
        }
        for rel in ["", ".", "./share", "share/", "share//x", "share/./x"] {
            assert_eq!(
                refused_pair("/usr/share/x", rel),
                PathsErrorKind::Unnormalized,
                "{rel:?}"
            );
        }
    }

    #[test]
    fn a_control_character_is_refused_on_either_side() {
        assert_eq!(
            refused_pair("/usr/share/x\0y", "share/x"),
            PathsErrorKind::ControlCharacter
        );
        assert_eq!(
            refused_pair("/usr/share/x", "share/x\0"),
            PathsErrorKind::ControlCharacter
        );
        assert_eq!(
            encode(&[pair("/usr/share/x\ny", "share/x")])
                .unwrap_err()
                .kind(),
            PathsErrorKind::ControlCharacter
        );
        assert_eq!(
            encode(&[pair("/usr/share/x", "share\tx")])
                .unwrap_err()
                .kind(),
            PathsErrorKind::ControlCharacter
        );
        assert_eq!(
            refused_pair("/usr/share/x\u{85}", "share/x"),
            PathsErrorKind::ControlCharacter
        );
        assert_eq!(
            refused_pair("/usr/share/x", "share/\u{7f}"),
            PathsErrorKind::ControlCharacter
        );
    }

    #[test]
    fn a_target_is_paired_once() {
        let twice = [
            pair("/usr/share/x", "share/a"),
            pair("/usr/share/x", "share/b"),
        ];
        assert_eq!(
            encode(&twice).unwrap_err().kind(),
            PathsErrorKind::DuplicateTarget
        );
        assert_eq!(
            refused(b"/usr/share/x\tshare/a\n/usr/share/x\tshare/b"),
            PathsErrorKind::DuplicateTarget
        );
    }

    #[test]
    fn path_lengths_are_bounded() {
        let longest = format!("/{}", "a".repeat(MAX_ABSOLUTE_BYTES - 1));
        assert!(encode(&[pair(&longest, "share/x")]).is_ok());
        assert_eq!(
            refused_pair(&format!("{longest}b"), "share/x"),
            PathsErrorKind::AbsoluteTooLong
        );
        let longest = "a".repeat(MAX_RELATIVE_BYTES);
        assert!(encode(&[pair("/usr/share/x", &longest)]).is_ok());
        assert_eq!(
            refused_pair("/usr/share/x", &format!("{longest}b")),
            PathsErrorKind::RelativeTooLong
        );
    }

    #[test]
    fn the_pair_count_is_bounded() {
        let pairs: Vec<PathPair> = (0..=MAX_PAIRS)
            .map(|i| pair(&format!("/t/{i}"), "share/x"))
            .collect();
        let bytes = encode(&pairs[..MAX_PAIRS]).unwrap();
        assert_eq!(decode(&bytes).unwrap().len(), MAX_PAIRS);
        assert_eq!(
            encode(&pairs).unwrap_err().kind(),
            PathsErrorKind::TooManyPairs
        );
        let mut over = bytes;
        over.extend_from_slice(b"\n/t/last\tshare/x");
        assert_eq!(refused(&over), PathsErrorKind::TooManyPairs);
    }

    #[test]
    fn the_record_size_is_bounded() {
        let relative = "r".repeat(MAX_RELATIVE_BYTES);
        let pairs: Vec<PathPair> = (0..MAX_PAIRS)
            .map(|i| pair(&format!("/{i:0>1000}"), &relative))
            .collect();
        assert_eq!(
            encode(&pairs).unwrap_err().kind(),
            PathsErrorKind::RecordTooLarge
        );
        let oversized = vec![b'/'; MAX_RECORD_BYTES + 1];
        assert_eq!(refused(&oversized), PathsErrorKind::RecordTooLarge);
    }

    #[test]
    fn a_record_of_exactly_the_size_bound_decodes() {
        let line = |i: usize, len: usize| format!("/{i:03}{}\tr", "a".repeat(len - 6));
        let lines = MAX_RECORD_BYTES.div_ceil(4096);
        let separators = lines - 1;
        let mut text: Vec<String> = (0..lines - 1).map(|i| line(i, 4095)).collect();
        let last = MAX_RECORD_BYTES - separators - 4095 * (lines - 1);
        text.push(line(lines - 1, last));
        let bytes = text.join("\n").into_bytes();
        assert_eq!(bytes.len(), MAX_RECORD_BYTES);
        let pairs = decode(&bytes).unwrap();
        assert_eq!(encode(&pairs).unwrap(), bytes);

        let mut over = bytes;
        over.push(b'r');
        assert_eq!(refused(&over), PathsErrorKind::RecordTooLarge);
        let mut pairs = pairs;
        pairs.last_mut().unwrap().relative.push('r');
        assert_eq!(
            encode(&pairs).unwrap_err().kind(),
            PathsErrorKind::RecordTooLarge
        );
    }

    #[test]
    fn a_line_level_error_names_the_line() {
        assert_eq!(
            decode(b"/a\tb\n").unwrap_err().to_string(),
            "line 2: the line holds no pair"
        );
        assert_eq!(
            decode(b"/a\tb\n/c").unwrap_err().to_string(),
            "line 2: the line has no tab between its paths"
        );
    }

    #[test]
    fn an_error_names_the_pair_it_refuses() {
        let err = encode(&[
            pair("/usr/share/ok", "share/ok"),
            pair("/usr/share/x", "../outside"),
        ])
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "\"/usr/share/x\" = \"../outside\": the bundle path leaves the package"
        );
        assert_eq!(
            decode(b"/usr/share/ok\tshare/ok\n/usr/share/x\t../outside")
                .unwrap_err()
                .to_string(),
            "line 2, \"/usr/share/x\" = \"../outside\": the bundle path leaves the package"
        );
        let err = decode(b"/usr/share/x\tshare/\x01").unwrap_err();
        assert!(err.to_string().contains("share/\\u{1}"), "{err}");
        let long = "/".to_string() + &"a".repeat(10_000);
        let err = encode(&[pair(&long, "share/x")]).unwrap_err();
        assert!(err.to_string().len() < 400, "{err}");
    }
}
