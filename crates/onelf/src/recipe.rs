//! Declarative recipe file (`onelf.toml`) for reproducible packs.
//!
//! Example:
//!
//! ```toml
//! [package]
//! name = "myapp"
//! command = "bin/myapp"
//! working-dir = "package"
//!
//! [[entrypoint]]
//! name = "myapp-cli"
//! path = "bin/myapp"
//! args = ["--no-gui"]
//!
//! [compression]
//! level = 12
//! dict = true
//!
//! [update]
//! url = "https://example.com/myapp.onelf.zsync"
//!
//! [bundle]
//! search-paths = ["${MUSL_LIBDRM}/lib"]
//! strict-libc = true
//! gl = true
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use onelf_format::WorkingDir;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Recipe {
    pub package: Package,
    #[serde(default)]
    pub entrypoint: Vec<Entrypoint>,
    #[serde(default)]
    pub compression: Compression,
    pub update: Option<Update>,
    #[serde(default)]
    pub bundle: Bundle,
    /// Take the bundle's contents from a pinned sysroot instead of
    /// scanning the packer's machine.
    pub sysroot: Option<Sysroot>,
    /// Custom environment variables set before exec. Values support
    /// `${ONELF_DIR}` which expands to the package root at runtime.
    /// Deserialized from the `[env]` TOML table into a `BTreeMap` so the
    /// emitted `.onelf/env` order is deterministic (sorted by key) across
    /// runs. Order carries no runtime meaning: each `KEY=VALUE` is applied
    /// independently and values expand against the live environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Libraries dlopen'd on every exec by the bundled onelf-env
    /// constructor. Paths support `${ONELF_DIR}`. Survives sandboxed
    /// re-exec (DT_NEEDED + $ORIGIN RUNPATH), unlike `LD_PRELOAD`.
    #[serde(default)]
    pub preload: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Package {
    pub name: Option<String>,
    pub command: String,
    pub output: Option<PathBuf>,
    #[serde(default)]
    pub working_dir: WorkingDirSpec,
    /// What the launch resolver may take from the host. `auto` (the
    /// default) decides from the bundle's contents.
    pub host_libs: Option<HostLibsSpec>,
    /// Let the runtime fall back to the persistent cache when no other
    /// execution mode works. Off by default, so a package leaves nothing
    /// on disk unless the publisher or the user asks for it.
    #[serde(default)]
    pub cache: bool,
    /// Mark the default entrypoint memfd-eligible (overrides auto-detect).
    pub memfd: Option<bool>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Pin every entry's mtime to this Unix timestamp for reproducible
    /// output. Overrides filesystem mtimes and `SOURCE_DATE_EPOCH`.
    pub mtime: Option<u64>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    /// Whether the app runs setuid binaries, such as an app that installs
    /// packages through `sudo` or `pkexec`.
    ///
    /// A setuid bit means nothing inside a user namespace the caller made
    /// itself: the owning uid is unmapped, so the file reads as `nobody` and
    /// the binary refuses. Saying so here keeps the app out of a namespace,
    /// at the cost of the modes that need one.
    #[serde(default)]
    pub needs_setuid: bool,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkingDirSpec {
    #[default]
    Inherit,
    Package,
    Command,
}

impl From<WorkingDirSpec> for WorkingDir {
    fn from(s: WorkingDirSpec) -> Self {
        match s {
            WorkingDirSpec::Inherit => WorkingDir::Inherit,
            WorkingDirSpec::Package => WorkingDir::PackageRoot,
            WorkingDirSpec::Command => WorkingDir::EntrypointParent,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Entrypoint {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Compression {
    #[serde(default = "default_level")]
    pub level: i32,
    /// Bytes per payload block. See [`crate::compress::DEFAULT_BLOCK_SIZE`].
    #[serde(default = "default_block_size")]
    pub block_size: u64,
    #[serde(default)]
    pub dict: bool,
    /// Store the payload uncompressed (no zstd). Overrides `dict` and
    /// `level`. Larger file, zero decompression at runtime.
    #[serde(default)]
    pub store: bool,
}

impl Default for Compression {
    fn default() -> Self {
        Self {
            level: default_level(),
            block_size: default_block_size(),
            dict: false,
            store: false,
        }
    }
}

fn default_level() -> i32 {
    12
}

fn default_block_size() -> u64 {
    crate::compress::DEFAULT_BLOCK_SIZE
}

/// Recipe spelling of the host library policy.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostLibsSpec {
    Auto,
    Always,
    Never,
}

impl From<HostLibsSpec> for crate::pack::HostLibs {
    fn from(v: HostLibsSpec) -> Self {
        match v {
            HostLibsSpec::Auto => crate::pack::HostLibs::Auto,
            HostLibsSpec::Always => crate::pack::HostLibs::Always,
            HostLibsSpec::Never => crate::pack::HostLibs::Never,
        }
    }
}

/// `[sysroot]`: a materialized rootfs whose package database decides the
/// bundle's contents. Paths are relative to the recipe.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Sysroot {
    /// The materialized rootfs.
    pub path: PathBuf,
    /// An archive to materialize into `path` when it does not exist yet.
    pub archive: Option<PathBuf>,
    /// A label for this sysroot, recorded in the package's provenance.
    /// Defaults to the archive's file name, or the directory's name.
    pub platform: Option<String>,
    /// Optional dependencies to include, by package name.
    #[serde(default)]
    pub optional: Vec<String>,
    /// File of soname prefixes the host provides, one per line.
    pub platform_line: Option<PathBuf>,
    /// File of glob patterns that never ship, one per line.
    pub policy: Option<PathBuf>,
    /// File of paths a test run opened, one per line.
    pub trace: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Update {
    pub url: String,
    /// Path to a file with the raw 32-byte Ed25519 public key used to
    /// verify signed self-updates.
    pub key: Option<PathBuf>,
    /// Embed the update-capable runtime. Defaults to true. False records
    /// the URL and key but links the slim runtime, for packages updated
    /// by a package manager rather than by themselves.
    pub embed: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Bundle {
    /// If absent, defaults to ["auto"]. An empty list disables bundling.
    pub lib_dirs: Option<Vec<String>>,
    #[serde(default)]
    pub search_paths: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub gl: bool,
    #[serde(default)]
    pub dri: bool,
    #[serde(default)]
    pub vulkan: bool,
    #[serde(default)]
    pub wayland: bool,
    #[serde(default)]
    pub gtk: bool,
    /// Opt out of a framework even when auto-detection would enable it.
    #[serde(default)]
    pub no_gl: bool,
    #[serde(default)]
    pub no_dri: bool,
    #[serde(default)]
    pub no_vulkan: bool,
    #[serde(default)]
    pub no_wayland: bool,
    #[serde(default)]
    pub no_gtk: bool,
    #[serde(default)]
    pub strip: bool,
    #[serde(default)]
    pub strict_libc: bool,
    #[serde(default)]
    pub scan_dlopen: bool,
    /// Extra sonames added to the --scan-dlopen allow-list.
    #[serde(default)]
    pub dlopen: Vec<String>,
    /// Skip running bundle-libs entirely (e.g. pre-bundled AppDir).
    #[serde(default)]
    pub skip: bool,
}

/// Load a recipe from a file path. `${VAR}` references anywhere in
/// the TOML text are expanded from environment variables before
/// parsing, so any field can use them:
///
/// ```toml
/// [package]
/// version = "${PKG_VERSION}"
/// ```
///
/// Missing variables expand to an empty string (matches shell
/// default). Returns an io::Error with a descriptive message on
/// failure (parse errors are converted via `InvalidData`).
pub fn load(path: &Path) -> std::io::Result<Recipe> {
    let raw = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidData => std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{}: not a valid recipe (expected a TOML file)",
                path.display()
            ),
        ),
        _ => std::io::Error::new(e.kind(), format!("{}: {e}", path.display())),
    })?;
    // Parse first, then expand `${VAR}` inside string *values* only. Doing
    // it after the structure is fixed means an expanded value containing a
    // quote or newline can never introduce new recipe keys.
    let mut value: toml::Value = toml::from_str(&raw).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })?;
    expand_value(&mut value);
    value.try_into().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })
}

/// Recursively expand `${VAR}` references in every string value of a parsed
/// TOML document. Keys are left untouched, so expansion can only ever change
/// values, never the document structure.
fn expand_value(value: &mut toml::Value) {
    match value {
        toml::Value::String(s) => *s = expand_env(s),
        toml::Value::Array(a) => a.iter_mut().for_each(expand_value),
        toml::Value::Table(t) => t.iter_mut().for_each(|(_, v)| expand_value(v)),
        _ => {}
    }
}

/// Resolve a recipe file from a user-supplied spec:
/// - If `spec` is a directory, use `<spec>/onelf.toml`.
/// - If `spec` is a file with a `.toml` extension, use it.
/// - Otherwise error, since the input is not something this tool can read.
pub fn resolve(spec: &Path) -> std::io::Result<PathBuf> {
    if spec.is_dir() {
        return Ok(spec.join("onelf.toml"));
    }
    if !spec.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{}: no such file or directory", spec.display()),
        ));
    }
    if spec.extension().and_then(|s| s.to_str()) != Some("toml") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{}: expected a directory containing onelf.toml or a .toml file",
                spec.display()
            ),
        ));
    }
    Ok(spec.to_path_buf())
}

/// Expand `${VAR}` references in a string from the **packer's**
/// environment at recipe-load time. Variables not set are preserved
/// as-is (e.g. `${ONELF_DIR}` stays literal so the runtime can expand
/// it later).
///
/// `$$` is an escape for a literal `$`: it is emitted as a single `$`
/// and never triggers pack-time expansion. This lets a recipe defer a
/// reference to **runtime**, e.g. `PATH = "${ONELF_DIR}/bin:$${PATH}"`
/// reaches `.onelf/env` as `${ONELF_DIR}/bin:${PATH}` and the runtime
/// (onelf-rt / the onelf-env constructor) expands `${PATH}` against the
/// live process environment, so it prepends instead of replacing.
pub fn expand_env(s: &str) -> String {
    let bytes = s.as_bytes();
    // Accumulate raw bytes: the input is UTF-8 and every spliced fragment
    // (env values and preserved source slices) is UTF-8, so the result is
    // valid UTF-8. This avoids the `bytes[i] as char` path that mangled
    // multi-byte sequences into mojibake.
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        // `$$` -> literal `$` (escape; defers any following `{VAR}` to
        // runtime since the `${` pattern is no longer present).
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            out.push(b'$');
            i += 2;
            continue;
        }
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'{'
            && let Some(end) = bytes[i + 2..].iter().position(|&b| b == b'}')
        {
            let name = std::str::from_utf8(&bytes[i + 2..i + 2 + end]).unwrap_or("");
            match std::env::var(name) {
                Ok(val) => out.extend_from_slice(val.as_bytes()),
                // Preserve unset variables for runtime expansion.
                Err(_) => out.extend_from_slice(&bytes[i..i + 3 + end]),
            }
            i += 3 + end;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod package_tests {
    use super::load;

    fn load_str(body: &str) -> super::Recipe {
        let dir = std::env::temp_dir().join(format!(
            "onelf-recipe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("onelf.toml");
        std::fs::write(&path, body).unwrap();
        let recipe = load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        recipe
    }

    #[test]
    fn a_sysroot_section_parses_with_relative_paths() {
        let recipe = load_str(
            "[package]\ncommand = \"bin/app\"\n\n[sysroot]\npath = \"sysroot\"\narchive = \"root.tar.zst\"\noptional = [\"extra\"]\nplatform-line = \"platform.txt\"\npolicy = \"policy.txt\"\n",
        );
        let sr = recipe.sysroot.expect("section present");
        assert_eq!(sr.path, std::path::PathBuf::from("sysroot"));
        assert_eq!(
            sr.archive.as_deref(),
            Some(std::path::Path::new("root.tar.zst"))
        );
        assert_eq!(sr.optional, ["extra"]);
        assert!(sr.trace.is_none());
        assert!(sr.platform.is_none());
        let recipe = load_str(
            "[package]\ncommand = \"bin/app\"\n\n[sysroot]\npath = \"sysroot\"\nplatform = \"platform-1\"\n",
        );
        assert_eq!(
            recipe.sysroot.unwrap().platform.as_deref(),
            Some("platform-1")
        );
        assert!(
            load_str("[package]\ncommand = \"bin/app\"\n")
                .sysroot
                .is_none()
        );
    }

    #[test]
    fn cache_defaults_off_and_parses_on() {
        let recipe = load_str("[package]\ncommand = \"bin/app\"\n");
        assert!(!recipe.package.cache);

        let recipe = load_str("[package]\ncommand = \"bin/app\"\ncache = true\n");
        assert!(recipe.package.cache);
    }
}

#[cfg(test)]
mod expand_tests {
    use super::expand_env;

    #[test]
    fn dollar_dollar_escapes_and_defers_to_runtime() {
        // $$ -> literal $
        assert_eq!(expand_env("a$$b"), "a$b");
        // $${VAR} must reach the output as ${VAR} (NOT pack-expanded),
        // so the runtime expands it -> PATH prepend instead of replace.
        assert_eq!(
            expand_env("${ONELF_DIR}/bin:$${PATH}"),
            "${ONELF_DIR}/bin:${PATH}"
        );
        // $$ + POSIX default must survive pack-time intact for runtime.
        assert_eq!(
            expand_env("${ONELF_DIR}/bin:$${PATH:-/usr/bin:/bin}"),
            "${ONELF_DIR}/bin:${PATH:-/usr/bin:/bin}"
        );
    }

    #[test]
    fn unset_vars_are_preserved_for_runtime() {
        assert_eq!(expand_env("x${ONELF_DIR}y"), "x${ONELF_DIR}y");
        assert_eq!(
            expand_env("${ONELF_THIS_IS_NOT_SET_4f3a}"),
            "${ONELF_THIS_IS_NOT_SET_4f3a}"
        );
    }

    #[test]
    fn expand_preserves_non_ascii() {
        // Non-ASCII in the literal (non-`${}`) parts must be copied
        // byte-for-byte; the old `bytes[i] as char` path mangled multi-byte
        // UTF-8 into mojibake. No process-env mutation needed: an unset
        // `${VAR}` is preserved verbatim, so the surrounding text carries
        // the assertion.
        assert_eq!(
            expand_env("café ☕ Ω ${ONELF_UNSET_9q}"),
            "café ☕ Ω ${ONELF_UNSET_9q}"
        );
    }
}
// The `${VAR}` injection-safety regression is covered by an integration
// test (`recipe_expansion_cannot_inject_keys` in tests/pipeline.rs), which
// sets the variable on a child process rather than mutating this process's
// environment.
