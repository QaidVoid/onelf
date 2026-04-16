//! Scaffold a starter `onelf.toml` recipe.

use std::io::{self, Write};
use std::path::Path;

/// Write a default onelf.toml at `output`. If `binary` is provided, its
/// basename is used for the package name and it becomes `bin/<name>`.
/// Refuses to overwrite an existing file unless `force` is set.
pub fn init(output: &Path, binary: Option<&Path>, force: bool) -> io::Result<()> {
    if output.exists() && !force {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists (use --force to overwrite)", output.display()),
        ));
    }

    let (name, command) = match binary {
        Some(b) => {
            let basename = b.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "--binary has no filename")
            })?;
            (basename.to_string(), format!("bin/{basename}"))
        }
        None => ("myapp".to_string(), "bin/myapp".to_string()),
    };

    let body = format!(
        r##"# onelf recipe. Edit to taste, then run `onelf build` to produce the binary.

[package]
name = "{name}"
command = "{command}"

# Optional output path (relative paths resolve against this file's directory).
# output = "{name}.onelf"

# Working directory for the entrypoint at runtime.
# "inherit" keeps the caller's cwd, "package" = package root, "command" = entrypoint's parent.
# working-dir = "inherit"

# Additional entrypoints. The binary is invoked under the matching name via
# argv[0] (create a symlink next to the onelf with that name).
# [[entrypoint]]
# name = "{name}-daemon"
# path = "bin/{name}"
# args = ["--daemon"]

[compression]
# Zstd compression level, 0..=22. Higher = smaller file, slower pack.
level = 12
# Train a shared dictionary across blocks for better ratios.
dict = false

# Uncomment to enable self-updates. Host the .zsync file (produced by
# `zsyncmake`) at this URL. Packages with updates enabled use a larger
# runtime (~2 MB extra).
# [update]
# url = "https://example.com/{name}.onelf.zsync"

[bundle]
# Pass ["auto"] to detect, or list directories to add to LD_LIBRARY_PATH.
# lib-dirs = ["auto"]

# Extra directories searched for shared libraries (highest priority).
# search-paths = ["${{MUSL_LIBDRM}}/lib"]

# Skip libraries whose libc family (musl vs glibc) doesn't match the target.
# strict-libc = false

# Scan binaries for strings matching a known allow-list of dlopen'd libs
# (GL/EGL/Wayland/Vulkan/audio/DBus/...) and bundle the matches.
# scan-dlopen = false

# Extra sonames added to the --scan-dlopen allow-list.
# dlopen = ["libmyvendor.so.1"]

# Framework bundlers (auto-enabled from DT_NEEDED if the binary links them).
# gl = false
# dri = false
# vulkan = false
# wayland = false
# gtk = false

# Strip debug symbols from bundled libs.
# strip = false

# Skip running bundle-libs entirely (for pre-bundled AppDirs).
# skip = false
"##
    );

    let mut f = std::fs::File::create(output)?;
    f.write_all(body.as_bytes())?;
    eprintln!("Wrote {}", output.display());
    Ok(())
}
