//! Validated framework defaults generated from the crate's canonical TOML sources.
//!
//! Generation parses and validates the complete catalog at build time. Runtime
//! access constructs typed definitions once and needs neither a TOML parser nor
//! the source files. Descriptor identities are sorted lexically. Candidate,
//! resource and hook ordering within each descriptor retains source order. The
//! directories compiled-in path inference never collects are generated the same
//! way, from their own source, with each table in source order.

use crate::rules::*;
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/framework_catalog.rs"));
include!(concat!(env!("OUT_DIR"), "/inference_exclusions.rs"));

/// Return the shared built-in framework descriptors in stable identity order.
///
/// These defaults describe exceptional resource discovery and presence-based
/// runtime configuration. Access does not discover files, activate descriptors,
/// execute hooks or modify the environment. Helper requirements apply only when
/// their associated resource group is selected. Version-dependent Tcl/Tk and
/// module-cache lookup remains in registered runtime hooks. Fontconfig handles
/// already carried fonts and does not request automatic host font copying.
pub fn builtins() -> &'static [Descriptor] {
    static CATALOG: OnceLock<Vec<Descriptor>> = OnceLock::new();
    CATALOG.get_or_init(generated_catalog)
}

/// Return the directories compiled-in path inference never collects.
///
/// The table is validated when the crate is built, so access needs no parser.
pub fn inference_exclusions() -> &'static InferenceExclusions {
    static EXCLUSIONS: OnceLock<InferenceExclusions> = OnceLock::new();
    EXCLUSIONS.get_or_init(generated_inference_exclusions)
}
