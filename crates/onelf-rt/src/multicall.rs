//! Entrypoint resolution via multicall-style dispatch.
//!
//! Resolves which entrypoint to execute based on (in priority order):
//! 1. `ONELF_ENTRYPOINT` environment variable, unless a runtime exported it
//! 2. `basename(argv[0])` matched against entrypoint names
//! 3. The manifest's default entrypoint index

use std::path::Path;

use onelf_format::Manifest;

/// The variable a runtime sets to the entrypoint it exported, so a
/// package the app launches tells an inherited `ONELF_ENTRYPOINT` apart
/// from one set on purpose.
pub const EXPORTED_VAR: &str = "ONELF_ENTRYPOINT_EXPORTED";

/// Resolve which entrypoint index to use.
pub fn resolve_entrypoint(manifest: &Manifest, argv0: &str) -> usize {
    let names: Vec<&str> = manifest
        .entrypoints
        .iter()
        .map(|ep| manifest.get_string(ep.name))
        .collect();
    pick(
        &names,
        manifest.header.default_entrypoint as usize,
        argv0,
        std::env::var("ONELF_ENTRYPOINT").ok().as_deref(),
        std::env::var(EXPORTED_VAR).ok().as_deref(),
    )
}

/// The entrypoint among `names` to run. `requested` is `ONELF_ENTRYPOINT`
/// and counts unless it is the value a runtime exported to this process,
/// which a launching package left behind rather than asked for.
fn pick(
    names: &[&str],
    default: usize,
    argv0: &str,
    requested: Option<&str>,
    exported: Option<&str>,
) -> usize {
    let find = |name: &str| names.iter().position(|n| *n == name);

    // 1. Check env override
    if let Some(name) = requested.filter(|name| Some(*name) != exported)
        && let Some(idx) = find(name)
    {
        return idx;
    }

    // 2. Match basename(argv[0])
    let basename = Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(argv0);
    if let Some(idx) = find(basename) {
        return idx;
    }

    // 3. Default entrypoint
    default
}

#[cfg(test)]
mod tests {
    use super::pick;

    const NAMES: &[&str] = &["app", "daemon"];

    #[test]
    fn a_requested_entrypoint_wins() {
        assert_eq!(pick(NAMES, 0, "/x/app.onelf", Some("daemon"), None), 1);
    }

    #[test]
    fn an_inherited_export_does_not_override_argv0() {
        // The app was started as `app` and re-runs itself as `daemon`:
        // the `app` it was handed is what it inherited, not a request.
        assert_eq!(pick(NAMES, 0, "/x/daemon", Some("app"), Some("app")), 1);
    }

    #[test]
    fn a_request_made_under_an_export_still_counts() {
        assert_eq!(
            pick(NAMES, 0, "/x/app.onelf", Some("daemon"), Some("app")),
            1
        );
    }
}
