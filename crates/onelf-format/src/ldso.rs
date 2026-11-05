//! Closing a glibc loader image to the host's filesystem.
//!
//! glibc's `ld-linux` bakes in the directories it searches when nothing
//! else names a library, together with its cache and preload files under
//! `/etc/`. Those are `/lib64/` and `/usr/lib64/` on Fedora;
//! `/lib/<triplet>/`, `/usr/lib/<triplet>/`, `/lib/` and `/usr/lib/` on
//! Debian; `/usr/lib/` on Arch; `/usr/lib64/` on Void;
//! and the loader's own `lib/` inside the store on Nix and Guix.
//! `--inhibit-cache` skips only the cache, so a loader run with an
//! explicit library path still falls back to those directories and can
//! load any library the host has there. No option turns that off. The
//! only way to close the search is to overwrite the strings themselves.
//!
//! Each path component that names one of those places is replaced with
//! `XXX`, padded with slashes to the same length, so every offset in the
//! image stays valid. A string that starts with one becomes a path under
//! `/XXX`, which exists on no sane system, and one inside a longer path,
//! such as a Guix store's `lib/`, ends in a component the store never
//! has. Either way each lookup the loader makes there fails cleanly.
//! Nothing here depends on where the image came from, so it closes any
//! glibc loader image, the one a package bundles or a copy of a host's.

use std::fmt;

/// The largest loader image accepted. A loader is a few hundred KiB, and
/// one carrying its debug sections stays under a few MiB, so anything
/// larger is not a loader and is refused before it is scanned.
pub const MAX_LOADER_BYTES: usize = 32 * 1024 * 1024;

/// The prefixes a glibc loader names the filesystem by, and their
/// equal-length replacements.
///
/// `/usr/`, `/lib/`, `/lib64/`, `/lib32/` and `/libx32/` close every
/// default search directory string, multiarch ones included, and the
/// `lib/` a store-built loader searches inside its own store path.
/// `/etc/` closes the cache and the preload list. `/nix/` closes the rest
/// of a Nix store path.
///
/// The loader also compares paths against short fragments of these
/// names, and a compiler can split such a fragment across instruction
/// immediates (Fedora's `_dl_dst_substitute` tests `/lib` and `64`
/// separately), where no needle matches. Those fragments survive, but
/// they only decide how a path is treated, such as whether it counts as
/// a trusted directory. They never name a directory the loader opens.
const GLIBC_REPLACEMENTS: &[(&[u8], &[u8])] = &[
    (b"/etc/", b"/XXX/"),
    (b"/usr/", b"/XXX/"),
    (b"/nix/", b"/XXX/"),
    (b"/lib/", b"/XXX/"),
    (b"/lib64/", b"/XXX///"),
    (b"/lib32/", b"/XXX///"),
    (b"/libx32/", b"/XXX////"),
];

/// The longest default search directory recognized, which bounds the run
/// of path bytes the scan keeps track of. A store directory such as
/// `/nix/store/<hash>-glibc-2.40-66/lib/` is the longest form.
const MAX_DEFAULT_DIR_LEN: usize = 128;

/// The length of a Nix or Guix store hash, the component before the first
/// `-` of a store entry's name.
const STORE_HASH_LEN: usize = 32;

/// What closing a loader image changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoaderScrub {
    /// How many default search directory strings the image carried and
    /// the scrub closed. glibc keeps more than one copy of its directory
    /// table, so this counts copies, not distinct directories, and is
    /// meant as a gate: zero means the image keeps its search directories
    /// somewhere the scan does not recognize, so nothing proves its
    /// search is closed.
    pub default_dirs: usize,
    /// How many prefixes were overwritten in total.
    pub replaced: usize,
}

/// Why an image could not be closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderScrubError {
    /// The image is larger than [`MAX_LOADER_BYTES`].
    TooLarge(usize),
    /// The image does not start with the ELF magic.
    NotElf,
}

impl fmt::Display for LoaderScrubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(len) => write!(
                f,
                "the image is {len} bytes, more than the {MAX_LOADER_BYTES} a loader may have"
            ),
            Self::NotElf => f.write_str("the image is not an ELF object"),
        }
    }
}

impl std::error::Error for LoaderScrubError {}

/// Close the glibc loader `image` in place: count the default search
/// directories it carries, then overwrite every path prefix that names
/// the host's filesystem. The image keeps its length.
pub fn scrub_glibc_loader(image: &mut [u8]) -> Result<LoaderScrub, LoaderScrubError> {
    if image.len() > MAX_LOADER_BYTES {
        return Err(LoaderScrubError::TooLarge(image.len()));
    }
    if !image.starts_with(b"\x7fELF") {
        return Err(LoaderScrubError::NotElf);
    }
    let default_dirs = count_default_dirs(image);
    let replaced = replace_equal_length(image, GLIBC_REPLACEMENTS);
    Ok(LoaderScrub {
        default_dirs,
        replaced,
    })
}

/// Overwrite every occurrence of each needle in `data` with its
/// replacement, in list order, and return how many were overwritten.
/// Each pair must have equal lengths. An empty needle is skipped.
///
/// # Panics
///
/// When a needle and its replacement differ in length: always in a debug
/// build, and in a release build when the needle occurs in `data`.
pub fn replace_equal_length(data: &mut [u8], pairs: &[(&[u8], &[u8])]) -> usize {
    let mut replaced = 0;
    for (needle, replacement) in pairs {
        debug_assert_eq!(needle.len(), replacement.len());
        let len = needle.len();
        if len == 0 {
            continue;
        }
        let mut i = 0;
        while i + len <= data.len() {
            if data[i..i + len] == **needle {
                data[i..i + len].copy_from_slice(replacement);
                replaced += 1;
                i += len;
            } else {
                i += 1;
            }
        }
    }
    replaced
}

/// How many NUL-terminated default search directories `image` carries.
/// A candidate is the run of path bytes ending at a NUL, so a directory
/// that is only the tail of a longer path does not count.
fn count_default_dirs(image: &[u8]) -> usize {
    let mut count = 0;
    let mut run_start = Some(0);
    for (i, &b) in image.iter().enumerate() {
        if b == 0 {
            if let Some(start) = run_start
                && is_default_dir(&image[start..i])
            {
                count += 1;
            }
            run_start = Some(i + 1);
        } else if !is_path_byte(b) {
            run_start = Some(i + 1);
        } else if run_start.is_some_and(|start| i - start >= MAX_DEFAULT_DIR_LEN) {
            run_start = None;
        }
    }
    count
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.' | b'+')
}

/// Whether `dir` is one of glibc's default search directories:
/// `[/usr]/lib[64|32|x32]/`, optionally followed by a multiarch triplet
/// such as `x86_64-linux-gnu/`, or a store-built glibc's own `lib/`.
fn is_default_dir(dir: &[u8]) -> bool {
    is_system_lib_dir(dir) || is_store_lib_dir(dir)
}

fn is_system_lib_dir(dir: &[u8]) -> bool {
    let rest = dir.strip_prefix(b"/usr").unwrap_or(dir);
    let Some(rest) = rest.strip_prefix(b"/lib") else {
        return false;
    };
    let rest = [&b"64"[..], b"32", b"x32"]
        .iter()
        .find_map(|suffix| rest.strip_prefix(*suffix))
        .unwrap_or(rest);
    let Some(rest) = rest.strip_prefix(b"/") else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    rest.strip_suffix(b"/").is_some_and(is_multiarch_triplet)
}

/// `/<root>/store/<hash>-glibc-<version>/lib/`, the directory a glibc
/// built by Nix (`/nix/store`) or Guix (`/gnu/store`) searches.
fn is_store_lib_dir(dir: &[u8]) -> bool {
    let Some(rest) = dir.strip_prefix(b"/") else {
        return false;
    };
    let mut parts = rest.split(|&b| b == b'/');
    let (Some(root), Some(b"store"), Some(entry), Some(b"lib"), Some(b""), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let lower_alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let Some((hash, name)) = entry.split_at_checked(STORE_HASH_LEN) else {
        return false;
    };
    !root.is_empty()
        && root.iter().all(u8::is_ascii_lowercase)
        && hash.iter().all(lower_alnum)
        && name.starts_with(b"-glibc-")
}

fn is_multiarch_triplet(name: &[u8]) -> bool {
    name.windows(7).any(|w| w == b"-linux-")
        && name
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn image(body: &[u8]) -> Vec<u8> {
        let mut out = b"\x7fELF\x02\x01\x01\0".to_vec();
        out.extend_from_slice(body);
        out
    }

    fn scrubbed(body: &[u8]) -> (Vec<u8>, LoaderScrub) {
        let mut data = image(body);
        let outcome = scrub_glibc_loader(&mut data).unwrap();
        (data, outcome)
    }

    /// Every distribution layout the loader is built with: the default
    /// directories are found and none survives.
    #[test]
    fn each_layout_loses_its_default_directories() {
        let layouts: &[(&[u8], usize)] = &[
            (b"\0/lib64/\0/usr/lib64/\0", 2),
            (
                b"\0/lib/x86_64-linux-gnu/\0/usr/lib/x86_64-linux-gnu/\0/lib/\0/usr/lib/\0",
                4,
            ),
            (
                b"\0/lib/aarch64-linux-gnu/\0/usr/lib/aarch64-linux-gnu/\0/lib/\0/usr/lib/\0",
                4,
            ),
            (b"\0/usr/lib/\0", 1),
            (b"\0/lib32/\0/usr/lib32/\0", 2),
            (b"\0/libx32/\0/usr/libx32/\0", 2),
            (
                b"\x01/nix/store/0123456789abcdefghijklmnopqrstuv-glibc-2.40-66/lib/\0",
                1,
            ),
            (
                b"\0/gnu/store/0123456789abcdefghijklmnopqrstuv-glibc-2.39/lib/\0",
                1,
            ),
        ];
        for (body, dirs) in layouts {
            let (out, outcome) = scrubbed(body);
            assert_eq!(outcome.default_dirs, *dirs, "{body:?}");
            assert_eq!(out.len(), image(body).len());
            assert_eq!(count_default_dirs(&out), 0, "{out:?}");
            assert!(!out.windows(4).any(|w| w == b"/lib"), "{out:?}");
        }
    }

    #[test]
    fn the_cache_and_preload_files_are_closed_too() {
        let (out, outcome) =
            scrubbed(b"\0/etc/ld.so.cache\0/etc/ld.so.preload\0/usr/lib64/\0/lib64/\0");
        assert_eq!(outcome.default_dirs, 2);
        assert_eq!(
            &out[8..],
            b"\0/XXX/ld.so.cache\0/XXX/ld.so.preload\0/XXX/XXX///\0/XXX///\0"
        );
    }

    /// A directory string preceded by a byte that cannot be part of a
    /// path still counts, as on loaders whose directory table follows
    /// binary data directly.
    #[test]
    fn a_directory_after_binary_data_counts() {
        let (_, outcome) = scrubbed(b"\x80\x01/usr/lib64/\0");
        assert_eq!(outcome.default_dirs, 1);
    }

    #[test]
    fn paths_that_only_contain_a_default_directory_do_not_count() {
        for body in [
            &b"\0/usr/lib64/ld-linux-x86-64.so.2\0"[..],
            b"\0/opt/lib/\0",
            b"\0/usr/lib/locale\0",
            b"\0/lib/not a triplet/\0",
            b"\0/usr/lib/X86_64-linux-gnu/\0",
            b"\0/usr/lib/gcc/\0",
            b"/usr/lib",
            b"\0/nix/store/0123456789abcdefghijklmnopqrstuv-zlib-1.3/lib/\0",
            b"\0/nix/store/0123456789-glibc-2.40/lib/\0",
            b"\0/nix/store/0123456789abcdefghijklmnopqrstuv-glibc-2.40/lib/gconv/\0",
            b"\0/nix/store/0123456789abcdefghijklmnopqrstuv-glibc-2.40/\0",
            b"\0/nix/var/0123456789abcdefghijklmnopqrstuv-glibc-2.40/lib/\0",
            b"\0/Nix/store/0123456789abcdefghijklmnopqrstuv-glibc-2.40/lib/\0",
            b"\0//store/0123456789abcdefghijklmnopqrstuv-glibc-2.40/lib/\0",
        ] {
            let (_, outcome) = scrubbed(body);
            assert_eq!(outcome.default_dirs, 0, "{body:?}");
        }
    }

    /// A run of path bytes longer than any default directory is dropped
    /// rather than tracked, so the scan stays linear on hostile input.
    #[test]
    fn an_overlong_run_ending_in_a_directory_does_not_count() {
        let mut body = vec![b'a'; 4 * MAX_DEFAULT_DIR_LEN];
        body.extend_from_slice(b"/lib64/\0");
        let (_, outcome) = scrubbed(&body);
        assert_eq!(outcome.default_dirs, 0);
    }

    #[test]
    fn malformed_images_are_refused_untouched() {
        let mut text = b"/usr/lib64/\0".to_vec();
        assert_eq!(scrub_glibc_loader(&mut text), Err(LoaderScrubError::NotElf));
        assert_eq!(text, b"/usr/lib64/\0");
        assert_eq!(scrub_glibc_loader(&mut []), Err(LoaderScrubError::NotElf));
        assert_eq!(
            scrub_glibc_loader(&mut b"\x7fEL".to_vec()),
            Err(LoaderScrubError::NotElf)
        );
        let mut huge = image(&vec![0; MAX_LOADER_BYTES]);
        assert_eq!(
            scrub_glibc_loader(&mut huge),
            Err(LoaderScrubError::TooLarge(huge.len()))
        );
    }

    #[test]
    fn an_empty_needle_is_skipped() {
        let mut data = b"/usr/lib".to_vec();
        assert_eq!(replace_equal_length(&mut data, &[(b"", b"")]), 0);
        assert_eq!(data, b"/usr/lib");
    }

    /// Whether `loader` runs `args` to success. A file just written can
    /// still be held open for writing by a child another test thread is
    /// forking, which makes the exec fail with `ETXTBSY` until that child
    /// execs, so that one error is retried.
    fn runs(loader: &Path, args: &[&str]) -> bool {
        for _ in 0..100 {
            match std::process::Command::new(loader)
                .args(args)
                .env_remove("LD_LIBRARY_PATH")
                .env_remove("LD_PRELOAD")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
            {
                Ok(status) => return status.success(),
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => return false,
            }
        }
        false
    }

    /// The glibc loaders this machine has, when it has any, are closed and
    /// keep their ELF header. A file at a loader path that is not one,
    /// such as NixOS's stub loader or Alpine's gcompat, is skipped. One
    /// whose unmodified copy runs from the temporary directory still runs
    /// once closed, and no longer finds `/bin/true`'s libc without a
    /// library path where the unmodified copy finds it in its default
    /// directories. A loader for another architecture, or a temporary
    /// directory mounted `noexec`, leaves only the image checks.
    #[test]
    fn real_loaders_are_closed() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let candidates = [
            "/lib64/ld-linux-x86-64.so.2",
            "/usr/lib/ld-linux-x86-64.so.2",
            "/usr/lib64/ld-linux-x86-64.so.2",
            "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
            "/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
            "/lib/ld-linux-aarch64.so.1",
            "/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
            "/usr/aarch64-linux-gnu/usr/lib/ld-linux-aarch64.so.1",
            "/lib/ld-linux.so.2",
            "/usr/lib32/ld-linux.so.2",
        ];
        let dir = std::env::temp_dir().join(format!(
            "onelf-ldso-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let mut checked = 0;
        for (i, path) in candidates.iter().enumerate() {
            let Ok(original) = std::fs::read(path) else {
                continue;
            };
            let is_glibc_loader = original.len() >= 64
                && original.starts_with(b"\x7fELF")
                && original.windows(11).any(|w| w == b"ld.so.cache");
            if !is_glibc_loader {
                continue;
            }
            let mut data = original.clone();
            let outcome = scrub_glibc_loader(&mut data).unwrap();
            assert!(outcome.default_dirs > 0, "{path}");
            assert_eq!(data.len(), original.len(), "{path}");
            assert_eq!(count_default_dirs(&data), 0, "{path}");
            assert_eq!(data[..64], original[..64], "{path}");
            checked += 1;

            let place = |name: String, bytes: &[u8]| {
                let copy = dir.join(name);
                std::fs::write(&copy, bytes).unwrap();
                std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o700)).unwrap();
                copy
            };
            let unmodified = place(format!("ld-{i}.orig.so"), &original);
            if !runs(&unmodified, &["--version"]) {
                eprintln!("{path}: run checks skipped, an unmodified copy does not run here");
                continue;
            }
            let original_runs_true = runs(&unmodified, &["--inhibit-cache", "/bin/true"]);
            let copy = place(format!("ld-{i}.so"), &data);
            assert!(runs(&copy, &["--version"]), "{path} runs once closed");
            if original_runs_true {
                assert!(
                    !runs(&copy, &["--inhibit-cache", "/bin/true"]),
                    "{path} closed still finds a library in a default directory"
                );
            } else {
                eprintln!(
                    "{path}: closure check skipped, the unmodified copy cannot run /bin/true"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        if cfg!(all(
            target_os = "linux",
            target_env = "gnu",
            target_arch = "x86_64"
        )) {
            assert!(
                checked > 0,
                "a glibc x86_64 host has a loader at one of {candidates:?}"
            );
        }
    }
}
