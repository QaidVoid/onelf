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
//! level = 19
//! dict = true
//!
//! [update]
//! url = "https://example.com/myapp.onelf"
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
    /// Directories of the bundle made visible at the absolute paths an
    /// application compiled into itself. The key is the absolute path
    /// the application opens, the value is the directory in the bundle
    /// that should answer for it.
    #[serde(default)]
    pub paths: BTreeMap<String, String>,
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
    /// Run from the persistent cache before any other execution mode, for
    /// an app that records its own path in files that outlive it.
    /// Implies `cache`.
    #[serde(default)]
    pub prefer_cache: bool,
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
    /// Bytes per payload block, or the payload's default. See
    /// [`crate::pack::Payload::default_block_size`].
    pub block_size: Option<u64>,
    /// What the payload is made of. See [`crate::pack::Payload`].
    #[serde(default)]
    pub payload: PayloadSpec,
    #[serde(default)]
    pub dict: bool,
    /// Store the payload uncompressed (no zstd). Overrides `dict` and
    /// `level`. Larger file, zero decompression at runtime.
    #[serde(default)]
    pub store: bool,
    /// Startup profile recorded with `ONELF_PROFILE`, relative to the
    /// recipe. See [`crate::pack::read_profile`].
    pub prefetch: Option<PathBuf>,
}

impl Default for Compression {
    fn default() -> Self {
        Self {
            level: default_level(),
            block_size: None,
            payload: PayloadSpec::default(),
            dict: false,
            store: false,
            prefetch: None,
        }
    }
}

fn default_level() -> i32 {
    // 19 rather than 12: measured on a real bundle it is 6% smaller for
    // pack-time cost only, and 22 buys nothing further.
    19
}

/// Recipe spelling of the payload format.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PayloadSpec {
    #[default]
    Blocks,
    Stipple,
}

impl From<PayloadSpec> for crate::pack::Payload {
    fn from(v: PayloadSpec) -> Self {
        match v {
            PayloadSpec::Blocks => crate::pack::Payload::Blocks,
            PayloadSpec::Stipple => crate::pack::Payload::Stipple,
        }
    }
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
///
/// With `path` the bundle is the closure of the entrypoint's package.
/// Without one there is no package database, so the AppDir stays exactly
/// as the author assembled it and only the sets (and an explicit GL
/// build pin) are recorded for the runtime to fetch at launch.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Sysroot {
    /// The materialized rootfs, when the bundle is taken from a package
    /// database. Absent leaves the AppDir alone and records only the
    /// sets.
    pub path: Option<PathBuf>,
    /// An archive to materialize into `path` when it does not exist yet:
    /// a path relative to the recipe, or an `https://` URL.
    pub archive: Option<PathBuf>,
    /// The archive's BLAKE3 hash. When set, an archive that does not
    /// match is not unpacked.
    pub hash: Option<String>,
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
    /// File of glob patterns the trace may not prune.
    pub keep: Option<PathBuf>,
    /// URL of the GL build for hosts without one, overriding what the
    /// sysroot's `etc/onelf/platform.toml` names.
    pub platform_url: Option<String>,
    /// BLAKE3 hash of that build, overriding the sysroot's.
    pub platform_hash: Option<String>,
    /// Dependency sets shared between packages: sysroot packages whose
    /// closure the bundle leaves out and a pinned build supplies.
    #[serde(default)]
    pub sets: BTreeMap<String, SharedSet>,
}

impl Sysroot {
    /// Reject a `[sysroot]` that cannot do anything, and sysroot-database
    /// keys used without the database they belong to.
    ///
    /// Without `path` there is no package database: no closure to take
    /// the bundle from, no archive to materialize, no owner to look up.
    /// The AppDir then stands as the author assembled it, and the only
    /// things left to record are the sets and an explicit GL build pin.
    pub fn validate(&self) -> std::io::Result<()> {
        if self.path.is_some() {
            return Ok(());
        }
        let database_keys: &[(&str, bool)] = &[
            ("archive", self.archive.is_some()),
            ("hash", self.hash.is_some()),
            ("platform", self.platform.is_some()),
            ("optional", !self.optional.is_empty()),
            ("platform-line", self.platform_line.is_some()),
            ("policy", self.policy.is_some()),
            ("trace", self.trace.is_some()),
            ("keep", self.keep.is_some()),
        ];
        for (name, present) in database_keys {
            if *present {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "[sysroot] {name} is a sysroot-database concept and needs path: \
                         without it there is no package database to apply it to"
                    ),
                ));
            }
        }
        if self.sets.is_empty() && self.platform_url.is_none() && self.platform_hash.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "[sysroot] names neither path nor sets: there is nothing to record",
            ));
        }
        Ok(())
    }
}

/// One shared dependency set: the sysroot packages it holds, made into a
/// build with `onelf sysroot pack-set`, and where that build is fetched
/// from, pinned by hash.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SharedSet {
    pub packages: Vec<String>,
    pub url: String,
    pub blake3: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Update {
    pub url: String,
    /// Path to a file with the raw 32-byte Ed25519 public key used to
    /// verify signed self-updates.
    pub key: Option<PathBuf>,
    /// Release number stamped into the package as
    /// `.onelf/update-revision`. The runtime refuses downloads that are
    /// not strictly newer than the running revision.
    pub revision: Option<u64>,
    /// Accept unsigned self-updates: the package stores no signing key
    /// and its runtime installs whatever the update URL serves,
    /// warning on stderr every time. HTTPS is still required.
    #[serde(default)]
    pub unsigned: bool,
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
    /// Run the command and bundle every library it loaded at runtime.
    #[serde(default)]
    pub trace_run: bool,
    /// How long the traced run may go before it is stopped; 5 by default.
    #[serde(default)]
    pub trace_seconds: Option<u64>,
    /// Arguments for the traced run.
    #[serde(default)]
    pub trace_args: Vec<String>,
    /// Skip running bundle-libs entirely (e.g. pre-bundled AppDir).
    #[serde(default)]
    pub skip: bool,
}

/// Load a recipe from a file path. `${VAR}` references in string
/// values are expanded from environment variables after parsing, so
/// any string field can use them:
///
/// ```toml
/// [package]
/// version = "${PKG_VERSION}"
/// ```
///
/// A variable that is not set is kept as written, so the runtime can
/// still expand it (see [`expand_env`]). Returns an io::Error with a
/// descriptive message on failure (parse errors are converted via
/// `InvalidData`).
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
    let recipe: Recipe = value.try_into().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })?;
    if let Some(sr) = &recipe.sysroot {
        sr.validate().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        })?;
    }
    Ok(recipe)
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

    fn load_result(body: &str) -> std::io::Result<super::Recipe> {
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
        let recipe = load(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        recipe
    }

    fn load_str(body: &str) -> super::Recipe {
        load_result(body).unwrap()
    }

    #[test]
    fn a_sysroot_section_parses_with_relative_paths() {
        let recipe = load_str(
            "[package]\ncommand = \"bin/app\"\n\n[sysroot]\npath = \"sysroot\"\narchive = \"root.tar.zst\"\noptional = [\"extra\"]\nplatform-line = \"platform.txt\"\npolicy = \"policy.txt\"\n",
        );
        let sr = recipe.sysroot.expect("section present");
        assert_eq!(sr.path, Some(std::path::PathBuf::from("sysroot")));
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
        let sr = recipe.sysroot.unwrap();
        assert_eq!(sr.platform.as_deref(), Some("platform-1"));
        assert!(sr.platform_url.is_none() && sr.platform_hash.is_none());
        let recipe = load_str(
            "[package]\ncommand = \"bin/app\"\n\n[sysroot]\npath = \"sysroot\"\nplatform-url = \"https://e/gl.onelf\"\nplatform-hash = \"abc\"\n",
        );
        let sr = recipe.sysroot.unwrap();
        assert_eq!(sr.platform_url.as_deref(), Some("https://e/gl.onelf"));
        assert_eq!(sr.platform_hash.as_deref(), Some("abc"));
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
        assert!(!recipe.package.prefer_cache);

        let recipe = load_str("[package]\ncommand = \"bin/app\"\nprefer-cache = true\n");
        assert!(recipe.package.prefer_cache);
    }

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A `[sysroot]` with only sets has no package database: the AppDir
    /// is the author's own, and the sets are recorded for the runtime.
    #[test]
    fn a_sysroot_with_only_sets_parses() {
        let recipe = load_str(&format!(
            "[package]\ncommand = \"bin/app\"\n\n[sysroot.sets.gtk]\npackages = [\"gtk3\"]\nurl = \"https://e/gtk.onelf\"\nblake3 = \"{HASH}\"\n"
        ));
        let sr = recipe.sysroot.expect("section present");
        assert!(sr.path.is_none());
        assert_eq!(sr.sets["gtk"].packages, ["gtk3"]);
        assert_eq!(sr.sets["gtk"].url, "https://e/gtk.onelf");
        assert_eq!(sr.sets["gtk"].blake3, HASH);
    }

    #[test]
    fn a_sysroot_with_neither_path_nor_sets_is_refused() {
        let err = load_result("[package]\ncommand = \"bin/app\"\n\n[sysroot]\n").unwrap_err();
        assert!(err.to_string().contains("neither path nor sets"), "{err}");
    }

    /// Every key that names a package-database concept is a typo waiting
    /// to be ignored when there is no database to apply it to.
    #[test]
    fn sysroot_database_keys_are_refused_without_a_path() {
        let hash = HASH;
        let bodies = [
            "[sysroot]\narchive = \"root.tar.zst\"\n",
            "[sysroot]\nhash = \"abc\"\n",
            "[sysroot]\nplatform = \"platform-1\"\n",
            "[sysroot]\noptional = [\"mesa\"]\n",
            "[sysroot]\nplatform-line = \"line.txt\"\n",
            "[sysroot]\npolicy = \"policy.txt\"\n",
            "[sysroot]\ntrace = \"trace.txt\"\n",
            "[sysroot]\nkeep = \"keep.txt\"\n",
        ];
        for body in bodies {
            let err =
                load_result(&format!("[package]\ncommand = \"bin/app\"\n\n{body}")).unwrap_err();
            assert!(err.to_string().contains("needs path"), "{body}: {err}");
        }
        // The GL build pin keeps its meaning without a database: both
        // fields name a build to fetch, nothing more.
        let recipe = load_str(&format!(
            "[package]\ncommand = \"bin/app\"\n\n[sysroot]\nplatform-url = \"https://e/gl.onelf\"\nplatform-hash = \"{hash}\"\n"
        ));
        let sr = recipe.sysroot.unwrap();
        assert!(sr.path.is_none());
        assert_eq!(sr.platform_url.as_deref(), Some("https://e/gl.onelf"));
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
