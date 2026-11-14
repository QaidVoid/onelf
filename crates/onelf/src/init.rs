//! Scaffold a starter `onelf.toml` recipe.

use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

/// Write a default onelf.toml at `output`. If `binary` is provided, its
/// basename is used for the package name and it becomes `bin/<name>`,
/// with an `[input]` section naming the binary relative to the recipe,
/// so the scaffolded recipe actually builds. A binary that already sits
/// at `bin/<name>` beside the recipe gets no `[input]`. Refuses to
/// overwrite an
/// existing file unless `force` is set.
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
    // A binary already at the command path is the AppDir's own, which an
    // import would rewrite in place, so it gets no `[input]`.
    let input = match binary.map(|b| recipe_relative(output, b)) {
        Some(rel) if rel != Path::new(&command) => format!(
            "\n[input]\n# The bare binary imported to `command` before assembly. A path\n\
             # relative to this file.\nbinary = \"{}\"\n",
            toml_escape(&rel.to_string_lossy())
        ),
        _ => String::new(),
    };

    let body = format!(
        r##"# onelf recipe. Edit to taste, then run `onelf build` to produce the binary.

[package]
name = "{name}"
command = "{command}"
{input}
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

# Scan binaries for names on a known allow-list of dlopen'd libs
# (Wayland, X11, audio, DBus, fonts) and bundle the matches.
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

/// `binary` as a path relative to the recipe `output`'s directory,
/// computed lexically so it holds before either path exists. An
/// absolute `--binary` still lands in the recipe, spelled so the
/// recipe-relative resolution the build applies finds it.
fn recipe_relative(output: &Path, binary: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let base = lexical_absolute(&cwd, output.parent().unwrap_or(Path::new("")));
    let target = lexical_absolute(&cwd, binary);
    relative_to(&base, &target)
}

/// `path` made absolute against `base` and stripped of `.` and `..`
/// components, without touching the filesystem.
fn lexical_absolute(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            component => out.push(component.as_os_str()),
        }
    }
    out
}

/// `path` spelled relative to `base`, both absolute.
fn relative_to(base: &Path, path: &Path) -> PathBuf {
    let base: Vec<_> = base.components().collect();
    let path: Vec<_> = path.components().collect();
    let shared = base
        .iter()
        .zip(path.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = PathBuf::new();
    for _ in shared..base.len() {
        out.push("..");
    }
    for component in &path[shared..] {
        out.push(component.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Escape `s` for a TOML basic string: a file name holding a quote or a
/// backslash must not break the recipe.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{init, recipe_relative};
    use crate::assembly::AssemblyRequest;
    use crate::recipe;

    /// A temp directory unique per test and per call, so parallel tests
    /// never share state.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onelf-init-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

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

    /// A seed without `--binary` gains no `[input]`: existing behavior
    /// stays as it was.
    #[test]
    fn a_plain_seed_writes_no_input_section() {
        let dir = temp_dir("plain");
        let path = dir.join("onelf.toml");
        init(&path, None, false).unwrap();
        let recipe = recipe::load(&path).unwrap();
        assert!(recipe.input.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `init --binary` writes a recipe that builds: the input resolves,
    /// and planning yields the seeded binary as the import source.
    #[test]
    fn a_binary_seed_writes_a_buildable_import_recipe() {
        let dir = temp_dir("seed");
        let binary = dir.join("upstream/app");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        let recipe_path = dir.join("pkg/onelf.toml");
        std::fs::create_dir_all(recipe_path.parent().unwrap()).unwrap();
        init(&recipe_path, Some(&binary), false).unwrap();
        let recipe = recipe::load(&recipe_path).unwrap();
        let input = recipe.input.as_ref().expect("[input] seeded");
        assert_eq!(
            input.binary,
            Some(std::path::PathBuf::from("../upstream/app"))
        );
        assert_eq!(recipe.package.command, "bin/app");
        let request = AssemblyRequest::plan(&recipe_path, recipe, None).unwrap();
        // The planned source is spelled relative to the recipe's own
        // directory, so compare it by what it reaches.
        assert_eq!(
            std::fs::canonicalize(request.input.expect("planned import").source).unwrap(),
            std::fs::canonicalize(&binary).unwrap()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A binary already at the command path beside the recipe is the
    /// AppDir's own: the seed names it as the command and imports
    /// nothing, so the recipe builds instead of rewriting its source.
    #[test]
    fn a_binary_at_the_command_path_gets_no_import() {
        let dir = temp_dir("in-place");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/app"), b"#!/bin/sh\n").unwrap();
        let recipe_path = dir.join("onelf.toml");
        init(&recipe_path, Some(&dir.join("bin/app")), false).unwrap();
        let recipe = crate::recipe::load(&recipe_path).unwrap();
        assert_eq!(recipe.package.command, "bin/app");
        assert!(recipe.input.is_none());
        crate::assembly::AssemblyRequest::plan(&recipe_path, recipe, None).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A binary inside the recipe's directory stays a plain relative
    /// path, and a name with TOML-hostile characters survives.
    #[test]
    fn seeds_spell_the_input_path_for_the_recipe() {
        let dir = temp_dir("spell");
        std::fs::write(dir.join("app"), b"#!/bin/sh\n").unwrap();
        assert_eq!(
            recipe_relative(&dir.join("onelf.toml"), &dir.join("app")),
            std::path::PathBuf::from("app")
        );
        assert_eq!(super::toml_escape("we\"ird\\name"), "we\\\"ird\\\\name");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
