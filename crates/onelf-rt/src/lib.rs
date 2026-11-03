//! Shared launch support for the onelf runtime.
//!
//! The library holds the machinery one launch of a packed application
//! needs, whatever drives it: the runtime binary in this crate, and the
//! CLI's development run command, which prepares a launch of its own and
//! hands the same modules a verified view instead of a packed package.
//!
//! The surface is narrow. The pieces an outside caller drives are the
//! package loader ([`loader`]), the validated metadata gate ([`pkgmeta`]),
//! the entrypoint selection and dependency-set acquisition ([`multicall`],
//! [`sets`]), the pinned artifact store ([`platform`]), the library links
//! and execution views ([`links`]), the environment setup and the record
//! that restores the caller's environment for host programs ([`env`],
//! [`hostenv`]), the execution modes ([`ephemeral`], [`fuse`], [`rundir`],
//! [`launch`]), and the lifetime guardian that holds a launch's on-disk
//! claims ([`guardian`]). Everything else inside these modules stays
//! crate-private: it exists to serve the same launch, not to be driven
//! from outside.
//!
//! Acquisition never carries its own network stack. A caller that wants
//! `https://` acquisition supplies its own HTTPS operation through
//! [`platform::supply_https_fetch`] before the first acquisition; without
//! one, `https://` sources are refused and only `file://` sources work.
//! The `update` feature of this crate stays opt-in: it adds the updater
//! binary and its signing and HTTPS dependencies, and nothing in the
//! library's base dependencies requires it, so a slim runtime that links
//! this library never grows an updater or an HTTPS client through
//! feature unification.
//!
//! The binary in this crate keeps what is genuinely binary-only: the
//! embedded capability note, the packed binary's flag handlers
//! (inspection, desktop metadata, integration), the diagnostics, and the
//! updater behind the `update` feature.

pub mod cache;
pub mod env;
pub mod ephemeral;
pub mod fuse;
pub mod guardian;
pub mod hostenv;
pub mod launch;
pub mod links;
pub mod loader;
pub mod multicall;
pub mod paths;
pub mod pkgmeta;
pub mod platform;
pub mod portable;
pub mod rundir;
pub mod sets;

pub mod hostpaths;
pub(crate) mod interp;
pub(crate) mod providers;
pub(crate) mod selfextract;
pub(crate) mod ulexec;

/// Keeps the tests that start a process apart from the tests that expect
/// a file lock of theirs to be released. A child inherits every descriptor
/// open when it is forked, a lock that survives an exec among them, and a
/// lock test running beside it then finds its lock still taken.
#[cfg(test)]
pub(crate) mod test_locks {
    use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

    static FORKS: RwLock<()> = RwLock::new(());

    /// Held by a lock test for its whole run.
    pub(crate) fn locking() -> RwLockReadGuard<'static, ()> {
        FORKS.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Held by a test for as long as a child it started lives.
    pub(crate) fn forking() -> RwLockWriteGuard<'static, ()> {
        FORKS.write().unwrap_or_else(|e| e.into_inner())
    }
}
