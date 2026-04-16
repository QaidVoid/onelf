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
}

#[derive(Debug, Default, Deserialize)]
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
}

impl Default for Compression {
    fn default() -> Self {
        Self {
            level: default_level(),
            dict: false,
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
    /// Skip running bundle-libs entirely (e.g. pre-bundled AppDir).
    #[serde(default)]
    pub skip: bool,
}

/// Load a recipe from a file path. Returns an io::Error with a descriptive
/// message on failure (parse errors are converted via `InvalidData`).
pub fn load(path: &Path) -> std::io::Result<Recipe> {
    let text = std::fs::read_to_string(path)?;
    toml::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{path:?}: {e}")))
}

/// If `spec` is a directory, return `spec/onelf.toml`. Otherwise return `spec`.
pub fn resolve(spec: &Path) -> PathBuf {
    if spec.is_dir() {
        spec.join("onelf.toml")
    } else {
        spec.to_path_buf()
    }
}

/// Expand `${VAR}` references in a string from the environment.
/// Missing vars become empty (matches shell's default behavior).
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = bytes[i + 2..].iter().position(|&b| b == b'}') {
                let name = std::str::from_utf8(&bytes[i + 2..i + 2 + end]).unwrap_or("");
                if let Ok(val) = std::env::var(name) {
                    out.push_str(&val);
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
