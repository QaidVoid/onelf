//! Entrypoint resolution via multicall-style dispatch.
//!
//! Resolves which entrypoint to execute based on (in priority order):
//! 1. `ONELF_ENTRYPOINT` environment variable
//! 2. `basename(argv[0])` matched against entrypoint names
//! 3. The manifest's default entrypoint index

use std::path::Path;

use onelf_format::Manifest;

/// Resolve which entrypoint index to use.
pub fn resolve_entrypoint(manifest: &Manifest, argv0: &str) -> usize {
    // 1. Check env override
    if let Ok(name) = std::env::var("ONELF_ENTRYPOINT") {
        if let Some(idx) = find_entrypoint_by_name(manifest, &name) {
            return idx;
        }
    }

    // 2. Match basename(argv[0])
    let basename = Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(argv0);

    if let Some(idx) = find_entrypoint_by_name(manifest, basename) {
        return idx;
    }

    // 3. Default entrypoint
    manifest.header.default_entrypoint as usize
}

fn find_entrypoint_by_name(manifest: &Manifest, name: &str) -> Option<usize> {
    manifest
        .entrypoints
        .iter()
        .position(|ep| manifest.get_string(ep.name) == name)
}
