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

use std::collections::HashMap;
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
    /// Custom environment variables set before exec. Values support
    /// `${ONELF_DIR}` which expands to the package root at runtime.
    #[serde(default)]
    pub env: HashMap<String, String>,
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
    /// Mark the default entrypoint memfd-eligible (overrides auto-detect).
    pub memfd: Option<bool>,
    #[serde(default)]
    pub exclude: Vec<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
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
            dict: false,
            store: false,
        }
    }
}

fn default_level() -> i32 {
    12
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Update {
    pub url: String,
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
    let text = expand_env(&raw);
    toml::from_str(&text).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    })
}

/// Resolve a recipe file from a user-supplied spec:
/// - If `spec` is a directory, use `<spec>/onelf.toml`.
/// - If `spec` is a file with a `.toml` extension, use it.
/// - Otherwise error — the input isn't something this tool can read as a recipe.
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

/// Expand `${VAR}` references in a string from the environment.
/// Variables not set in the environment are preserved as-is (e.g.
/// `${ONELF_DIR}` stays literal so the runtime can expand it later).
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = bytes[i + 2..].iter().position(|&b| b == b'}') {
                let name = std::str::from_utf8(&bytes[i + 2..i + 2 + end]).unwrap_or("");
                match std::env::var(name) {
                    Ok(val) => out.push_str(&val),
                    Err(_) => {
                        // Preserve unset variables for runtime expansion
                        out.push_str(&s[i..i + 3 + end]);
                    }
                }
                i += 3 + end;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
