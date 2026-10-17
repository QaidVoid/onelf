//! XDG desktop integration: install/remove .desktop files and icons.
//!
//! `integrate` extracts the icon and desktop file from a packed binary,
//! installs them to XDG-compliant paths, and patches the desktop file's
//! `Exec=` and `Icon=` fields. `unintegrate` reverses the process.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use onelf_format::desktop::{entry_icon, exec_command, icon_set, patch_desktop_file, string_value};
use onelf_format::{Footer, Manifest};

use crate::extract::decompress_verified;
use crate::info::read_footer_and_manifest;
use crate::metadata::{resolve_desktop, resolve_icon};

struct IntegrationContext {
    footer: Footer,
    manifest: Manifest,
    binary: PathBuf,
    pkg_name: String,
    ep_name: String,
    /// The entrypoint is not the package's default, so its files are
    /// named after it as well and do not replace the default's.
    secondary: bool,
    /// The entrypoint `Exec=` names, when the binary path alone would
    /// start a different one.
    exec_entrypoint: Option<String>,
}

impl IntegrationContext {
    fn load(binary: &Path, entrypoint: Option<&str>) -> io::Result<Self> {
        // The directory is resolved and the name kept: a symlink such as
        // `myapp-daemon -> myapp.onelf` is how an entrypoint is launched,
        // by its argv[0], and `Exec=` has to go through it.
        let binary = match (binary.parent(), binary.file_name()) {
            (Some(dir), Some(name)) => {
                let dir = if dir.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    dir
                };
                dir.canonicalize()?.join(name)
            }
            _ => binary.canonicalize()?,
        };
        let (footer, manifest) = read_footer_and_manifest(&binary)?;
        let launched = binary
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| {
                manifest
                    .entrypoints
                    .iter()
                    .position(|ep| manifest.get_string(ep.name) == n)
            })
            .unwrap_or(manifest.header.default_entrypoint as usize);

        let ep_name = match entrypoint {
            Some(name) => {
                // Validate that the entrypoint exists
                if !manifest
                    .entrypoints
                    .iter()
                    .any(|ep| manifest.get_string(ep.name) == name)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("entrypoint '{name}' not found"),
                    ));
                }
                name.to_string()
            }
            None => {
                let ep = manifest.entrypoints.get(launched).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "package declares no entrypoints",
                    )
                })?;
                manifest.get_string(ep.name).to_string()
            }
        };

        let ep_index = manifest
            .entrypoints
            .iter()
            .position(|ep| manifest.get_string(ep.name) == ep_name);
        // A path that would start another entrypoint names this one in
        // the launch instead.
        let exec_entrypoint = (ep_index != Some(launched)).then(|| ep_name.clone());
        let secondary = ep_index != Some(manifest.header.default_entrypoint as usize);

        let name = manifest.name();
        let pkg_name = if name.is_empty() {
            ep_name.clone()
        } else {
            name.to_string()
        };

        Ok(Self {
            footer,
            manifest,
            binary,
            pkg_name,
            ep_name,
            secondary,
            exec_entrypoint,
        })
    }

    fn read_entry(&self, entry_idx: usize) -> io::Result<Vec<u8>> {
        let entry = &self.manifest.entries[entry_idx];
        let mut file = fs::File::open(&self.binary)?;

        let dict = crate::info::read_dict(&mut file, &self.footer)?;

        decompress_verified(&mut file, &self.footer, entry, dict.as_deref())
    }
}

fn xdg_data_home() -> io::Result<PathBuf> {
    // A relative XDG_DATA_HOME is invalid per the basedir spec; honoring
    // it would scatter icons under the working directory. Fall through
    // to the HOME default instead.
    if let Some(val) = std::env::var_os("XDG_DATA_HOME") {
        let path = PathBuf::from(val);
        if path.is_absolute() {
            return Ok(path);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local/share"));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "neither a usable $XDG_DATA_HOME nor $HOME is set",
    ))
}

/// Sanitize the package name into a safe desktop entry name:
/// `onelf-{name}`, or `onelf-{name}-{entrypoint}` for an entrypoint other
/// than the default, so integrating a second entrypoint does not replace
/// the first.
fn integration_name(pkg_name: &str, entrypoint: Option<&str>) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    match entrypoint {
        Some(ep) => format!("onelf-{}-{}", sanitize(pkg_name), sanitize(ep)),
        None => format!("onelf-{}", sanitize(pkg_name)),
    }
}

impl IntegrationContext {
    fn integration_name(&self) -> String {
        integration_name(
            &self.pkg_name,
            self.secondary.then_some(self.ep_name.as_str()),
        )
    }
}

/// The bundled desktop file, if the package carries one.
fn bundled_desktop(ctx: &IntegrationContext) -> io::Result<Option<String>> {
    let Some(idx) = resolve_desktop(&ctx.manifest, &ctx.ep_name) else {
        return Ok(None);
    };
    String::from_utf8(ctx.read_entry(idx)?)
        .map(Some)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "desktop file is not valid UTF-8",
            )
        })
}

/// Install every icon size the package carries and return the icon theme
/// name for the `Icon=` field.
fn install_icons(
    ctx: &IntegrationContext,
    desktop: Option<&str>,
    data_home: &Path,
    int_name: &str,
) -> io::Result<Option<String>> {
    let mut names = vec![ctx.ep_name.as_str(), ctx.pkg_name.as_str()];
    if let Some(icon) = desktop.and_then(entry_icon) {
        names.insert(0, icon);
    }
    let bundled = resolve_icon(&ctx.manifest, &ctx.ep_name);
    let icons = icon_set(&ctx.manifest, &names, bundled, |i| ctx.read_entry(i))?;
    let installed = !icons.is_empty();
    for icon in icons {
        let dir = data_home
            .join("icons/hicolor")
            .join(&icon.size)
            .join("apps");
        fs::create_dir_all(&dir)?;
        let dest = dir.join(format!("{int_name}.{}", icon.ext));
        fs::write(&dest, &icon.data)?;
        eprintln!("  Icon: {}", dest.display());
    }
    Ok(installed.then(|| int_name.to_string()))
}

/// Install the desktop file, patching or generating as needed.
fn install_desktop(
    ctx: &IntegrationContext,
    desktop: Option<&str>,
    data_home: &Path,
    int_name: &str,
    icon_name: Option<&str>,
) -> io::Result<()> {
    let desktop_dir = data_home.join("applications");
    fs::create_dir_all(&desktop_dir)?;
    let dest = desktop_dir.join(format!("{int_name}.desktop"));

    let exec_path = ctx.binary.to_str().unwrap_or("");

    let content = match desktop {
        Some(text) => {
            patch_desktop_file(text, exec_path, ctx.exec_entrypoint.as_deref(), icon_name)
        }
        None => generate_desktop_file(
            &ctx.pkg_name,
            exec_path,
            ctx.exec_entrypoint.as_deref(),
            icon_name,
        ),
    };

    fs::write(&dest, content.as_bytes())?;
    eprintln!("  Desktop: {}", dest.display());
    Ok(())
}

/// Generate a minimal `.desktop` file.
fn generate_desktop_file(
    name: &str,
    exec_path: &str,
    entrypoint: Option<&str>,
    icon_name: Option<&str>,
) -> String {
    let quoted = exec_command(exec_path, entrypoint);
    let mut lines = vec![
        "[Desktop Entry]".to_string(),
        "Type=Application".to_string(),
        format!("Name={name}"),
        format!("Exec={quoted}"),
        // A literal path per the spec, not a quoted Exec field.
        format!("TryExec={}", string_value(exec_path)),
    ];
    if let Some(icon) = icon_name {
        lines.push(format!("Icon={icon}"));
    }
    lines.push("Terminal=false".to_string());
    lines.push(String::new());
    lines.join("\n")
}

/// Scan `hicolor/*/apps/` and remove icons matching `int_name`.
fn remove_icons(hicolor: &Path, int_name: &str) -> io::Result<bool> {
    let entries = match fs::read_dir(hicolor) {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };

    let target_svg = format!("{int_name}.svg");
    let target_png = format!("{int_name}.png");
    let mut removed = false;

    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let apps_dir = entry.path().join("apps");
        if !apps_dir.is_dir() {
            continue;
        }
        for name in [&target_svg, &target_png] {
            let path = apps_dir.join(name);
            if path.exists() {
                fs::remove_file(&path)?;
                eprintln!("  Removed: {}", path.display());
                removed = true;
            }
        }
    }

    Ok(removed)
}

fn update_desktop_database(data_home: &Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg(data_home.join("applications"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

pub fn integrate(binary: &Path, entrypoint: Option<&str>) -> io::Result<()> {
    let ctx = IntegrationContext::load(binary, entrypoint)?;
    let data_home = xdg_data_home()?;
    let int_name = ctx.integration_name();

    // Remove any stale icons first (handles format changes on re-integrate)
    let hicolor = data_home.join("icons/hicolor");
    if hicolor.exists() {
        let _ = remove_icons(&hicolor, &int_name);
    }

    let desktop = bundled_desktop(&ctx)?;
    let icon_name = install_icons(&ctx, desktop.as_deref(), &data_home, &int_name)?;
    install_desktop(
        &ctx,
        desktop.as_deref(),
        &data_home,
        &int_name,
        icon_name.as_deref(),
    )?;
    update_desktop_database(&data_home);

    eprintln!("Integrated '{}'", ctx.pkg_name);
    Ok(())
}

pub fn unintegrate(binary: &Path, entrypoint: Option<&str>) -> io::Result<()> {
    let ctx = IntegrationContext::load(binary, entrypoint)?;
    let data_home = xdg_data_home()?;
    let int_name = ctx.integration_name();

    let mut removed = false;

    let desktop_path = data_home
        .join("applications")
        .join(format!("{int_name}.desktop"));
    if desktop_path.exists() {
        fs::remove_file(&desktop_path)?;
        eprintln!("  Removed: {}", desktop_path.display());
        removed = true;
    }

    let hicolor = data_home.join("icons/hicolor");
    if hicolor.exists() {
        removed |= remove_icons(&hicolor, &int_name)?;
    }

    update_desktop_database(&data_home);

    if removed {
        eprintln!("Unintegrated '{}'", ctx.pkg_name);
    } else {
        eprintln!("Nothing was installed for '{}'", ctx.pkg_name);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_entrypoint_has_files_of_its_own() {
        assert_eq!(integration_name("My App", None), "onelf-My-App");
        assert_eq!(
            integration_name("My App", Some("my-daemon")),
            "onelf-My-App-my-daemon"
        );
    }

    #[test]
    fn patch_replaces_quoted_exec_without_fragments() {
        let desktop = "[Desktop Entry]\nType=Application\nExec=\"/opt/my app/bin\" %F\nIcon=old\n";
        let out = patch_desktop_file(desktop, "/new/path/app", None, Some("newicon"));
        // The old quoted path is gone (no leftover `app/bin"` fragment) and
        // the field code is preserved.
        assert!(out.contains("Exec=/new/path/app %F"), "got:\n{out}");
        assert!(!out.contains("app/bin"), "stale path fragment left:\n{out}");
        assert!(out.contains("Icon=newicon"));
    }

    #[test]
    fn generated_file_quotes_exec_path_with_spaces() {
        let out = generate_desktop_file("App", "/opt/has space/app", None, None);
        assert!(out.contains("Exec=\"/opt/has space/app\"\n"), "got:\n{out}");
        assert!(
            out.contains("TryExec=/opt/has space/app\n"),
            "TryExec is a literal path, not a quoted field: {out}"
        );
    }

    #[test]
    fn patch_quotes_exec_path_with_spaces() {
        let desktop = "[Desktop Entry]\nExec=/usr/bin/old %U\n";
        let out = patch_desktop_file(desktop, "/opt/has space/app", None, None);
        assert!(
            out.contains("Exec=\"/opt/has space/app\" %U"),
            "got:\n{out}"
        );
    }
}
