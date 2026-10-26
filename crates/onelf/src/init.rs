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

# Optional metadata. Displayed by `onelf info`, stored as .onelf/package-info.toml.
# version = "0.1.0"
# description = "Short summary of what this package does"
# license = "MIT"
# homepage = "https://example.com/{name}"

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
level = 19
# Train a shared dictionary across blocks for better ratios.
dict = false

# Uncomment to enable self-updates. Host the release binary at this URL;
# `onelf sign` names the signature to upload beside it. Packages with
# updates enabled use a larger runtime (about 1.3 MB more).
# [update]
# url = "https://example.com/{name}.onelf"
# key = "myapp.pub"
# revision = 1
# unsigned = true  # updates with no signature check; conflicts with key

[bundle]
# Pass ["auto"] to detect, or list the bundle's library directories.
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

# Framework bundlers. GL, DRI and Vulkan are opt-in, since the host's driver
# is the one that works: bundle Mesa only for hosts without a GL stack.
# gl = false
# dri = false
# vulkan = false
# Wayland and GTK data are also bundled when the binary needs them.
# wayland = false
# gtk = false

# Opt out of a framework even when auto-detection would enable it (useful
# for a GUI-capable binary you only ship as a TUI).
# no-gl = false
# no-dri = false
# no-vulkan = false
# no-wayland = false
# no-gtk = false

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

#[cfg(test)]
mod tests {
    /// The template writes `level` out, so it has to be the level a recipe
    /// without one gets.
    #[test]
    fn the_template_parses_and_keeps_the_default_level() {
        let dir = std::env::temp_dir().join(format!("onelf-init-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("onelf.toml");
        super::init(&path, None, true).unwrap();
        let written = crate::recipe::load(&path).unwrap();
        std::fs::write(&path, "[package]\ncommand = \"bin/myapp\"\n").unwrap();
        let bare = crate::recipe::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(written.compression.level, bare.compression.level);
    }
}
