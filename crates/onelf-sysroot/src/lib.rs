//! Pinned sysroots as a source of truth for what a bundle contains.
//!
//! A sysroot is a root filesystem with a package database. Every file in
//! it belongs to a package, and every package declares what it depends
//! on, so the set of files an application can reach is known before any
//! ELF is inspected. That is what a host scan cannot offer: a `dlopen` by
//! computed name, a plugin directory, a schema file are all invisible to
//! `DT_NEEDED`, and all recorded in the database.
//!
//! The crate reads the database, computes closures, materializes a
//! sysroot from an archive, and prunes a closure by the three tiers a
//! recipe may declare: the platform line the host supplies, a policy of
//! paths that never ship, and optionally the files a traced run never
//! opened.

pub mod archive;
pub mod closure;
pub mod db;
pub mod ldconf;
pub mod platform;
pub mod prune;

pub use closure::Closure;
pub use db::{Database, Package};
pub use platform::Pin;
pub use prune::{PlatformLine, Policy, Pruned, Trace};

/// Directories a distribution keeps shared libraries in, relative to a
/// sysroot root: the flat legacy dirs and Debian's multiarch triplets.
/// Package databases and traces name files under these without a leading
/// slash.
pub const LIB_DIRS: &[&str] = &[
    "usr/lib",
    "usr/lib64",
    "usr/lib32",
    "usr/lib/x86_64-linux-gnu",
    "usr/lib/aarch64-linux-gnu",
    "usr/lib/i386-linux-gnu",
    "usr/lib/arm-linux-gnueabihf",
    "lib",
    "lib64",
    "lib32",
    "lib/x86_64-linux-gnu",
    "lib/aarch64-linux-gnu",
    "lib/i386-linux-gnu",
    "lib/arm-linux-gnueabihf",
];

/// [`LIB_DIRS`] as absolute host paths.
pub const LIB_PATHS: &[&str] = &[
    "/usr/lib",
    "/usr/lib64",
    "/usr/lib32",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib/i386-linux-gnu",
    "/usr/lib/arm-linux-gnueabihf",
    "/lib",
    "/lib64",
    "/lib32",
    "/lib/x86_64-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/lib/i386-linux-gnu",
    "/lib/arm-linux-gnueabihf",
];

/// Whether `rel` (sysroot-relative) is a shared object sitting directly
/// in a distribution library directory.
///
/// A host package owning such a file serves the host, not the bundle:
/// its other files, Mesa's gallium and DRI drivers say, exist for that
/// library, and so does what only it depended on.
pub fn is_top_level_lib_object(rel: &str) -> bool {
    let Some((dir, name)) = rel.rsplit_once('/') else {
        return false;
    };
    LIB_DIRS.contains(&dir) && name.contains(".so")
}

#[cfg(test)]
mod lib_dirs_tests {
    use super::is_top_level_lib_object;

    #[test]
    fn top_level_libraries_in_flat_and_multiarch_dirs() {
        assert!(is_top_level_lib_object("usr/lib/libGL.so.1"));
        assert!(is_top_level_lib_object("usr/lib64/libEGL.so.1"));
        assert!(is_top_level_lib_object(
            "usr/lib/x86_64-linux-gnu/libEGL.so.1"
        ));
        assert!(is_top_level_lib_object("lib/aarch64-linux-gnu/libc.so.6"));
    }

    #[test]
    fn nested_or_non_library_files_are_not_top_level() {
        assert!(!is_top_level_lib_object("usr/lib/fixture/plugins/a.so"));
        assert!(!is_top_level_lib_object("usr/bin/app"));
        assert!(!is_top_level_lib_object("usr/share/doc/libfixture/README"));
        assert!(!is_top_level_lib_object("libEGL.so.1"));
    }
}
