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
pub mod platform;
pub mod prune;

pub use closure::Closure;
pub use db::{Database, Package};
pub use platform::Pin;
pub use prune::{PlatformLine, Policy, Pruned, Trace};
