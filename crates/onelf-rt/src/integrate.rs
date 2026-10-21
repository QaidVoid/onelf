//! XDG desktop integration from a running onelf binary.
//!
//! Handles `--onelf-integrate` and `--onelf-unintegrate` flags to install or
//! remove .desktop files and icons without needing the `onelf` CLI tool.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::loader::PackageData;
use crate::metadata::{resolve_desktop, resolve_icon};
use onelf_format::desktop::{entry_icon, icon_set, patch_desktop_file};

fn read_entry(pkg: &mut PackageData, entry_idx: usize) -> io::Result<Vec<u8>> {
    let blocks = pkg.manifest.entries[entry_idx].blocks.clone();
    crate::loader::read_payload_blocks(&pkg.file, &pkg.footer, &blocks, &pkg.codec)
}

fn xdg_data_home_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    // A relative XDG_DATA_HOME is invalid per the basedir spec; honoring
    // it would scatter icons under the working directory. Fall through
    // to the HOME default instead.
    if let Some(val) = xdg {
        let path = PathBuf::from(val);
        if path.is_absolute() {
            return Some(path);
        }
    }
    home.map(|h| PathBuf::from(h).join(".local/share"))
}

fn xdg_data_home() -> Option<PathBuf> {
    xdg_data_home_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// `onelf-{name}`, or `onelf-{name}-{entrypoint}` for an entrypoint other
/// than the default, matching the packer's `onelf integrate`.
fn integration_name(pkg: &PackageData, pkg_name: &str, ep_name: &str) -> String {
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
    let default = pkg
        .manifest
        .entrypoints
        .get(pkg.manifest.header.default_entrypoint as usize)
        .map(|ep| pkg.manifest.get_string(ep.name));
    if default == Some(ep_name) {
        format!("onelf-{}", sanitize(pkg_name))
    } else {
        format!("onelf-{}-{}", sanitize(pkg_name), sanitize(ep_name))
    }
}

fn remove_icons(hicolor: &Path, int_name: &str) -> bool {
    let entries = match fs::read_dir(hicolor) {
        Ok(e) => e,
        Err(_) => return false,
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
            if path.exists() && fs::remove_file(&path).is_ok() {
                eprintln!("  Removed: {}", path.display());
                removed = true;
            }
        }
    }
    removed
}

fn generate_desktop_file(
    name: &str,
    exec_path: &str,
    entrypoint: Option<&str>,
    icon_name: Option<&str>,
) -> String {
    let quoted = onelf_format::desktop::exec_command(exec_path, entrypoint);
    let mut lines = vec![
        "[Desktop Entry]".to_string(),
        "Type=Application".to_string(),
        format!("Name={name}"),
        format!("Exec={quoted}"),
        // A literal path per the spec, not a quoted Exec field.
        format!("TryExec={}", onelf_format::desktop::string_value(exec_path)),
    ];
    if let Some(icon) = icon_name {
        lines.push(format!("Icon={icon}"));
    }
    lines.push("Terminal=false".to_string());
    lines.push(String::new());
    lines.join("\n")
}

fn update_desktop_database(data_home: &Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg(data_home.join("applications"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn do_integrate(pkg: &mut PackageData, ep_name: &str, exec_path: &str) {
    // The runtime is integrated from the resolved package file, whose name
    // starts the default entrypoint or the one it is named after. Any
    // other entrypoint is named in the launch.
    let launched = Path::new(exec_path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| {
            pkg.manifest
                .entrypoints
                .iter()
                .any(|ep| pkg.manifest.get_string(ep.name) == *n)
        })
        .map(str::to_string)
        .or_else(|| {
            pkg.manifest
                .entrypoints
                .get(pkg.manifest.header.default_entrypoint as usize)
                .map(|ep| pkg.manifest.get_string(ep.name).to_string())
        });
    let exec_entrypoint = (launched.as_deref() != Some(ep_name)).then_some(ep_name);
    let Some(data_home) = xdg_data_home() else {
        eprintln!("onelf-rt: cannot determine XDG_DATA_HOME or HOME");
        std::process::exit(1);
    };

    let name = pkg.manifest.name();
    let pkg_name = if name.is_empty() {
        ep_name.to_string()
    } else {
        name.to_string()
    };
    let int_name = integration_name(pkg, &pkg_name, ep_name);

    // Clean stale icons first
    let hicolor = data_home.join("icons/hicolor");
    if hicolor.exists() {
        remove_icons(&hicolor, &int_name);
    }

    let desktop = resolve_desktop(&pkg.manifest, ep_name)
        .and_then(|idx| read_entry(pkg, idx).ok())
        .map(|raw| String::from_utf8(raw).unwrap_or_default());

    let mut names = vec![ep_name, pkg_name.as_str()];
    if let Some(icon) = desktop.as_deref().and_then(entry_icon) {
        names.insert(0, icon);
    }
    let bundled = resolve_icon(&pkg.manifest, ep_name);
    let read = |i: usize| {
        crate::loader::read_verified_entry(
            &pkg.file,
            &pkg.footer,
            &pkg.manifest.entries[i],
            &pkg.codec,
        )
    };
    let icons = icon_set(&pkg.manifest, &names, bundled, read).unwrap_or_else(|e| {
        eprintln!("onelf-rt: failed to read icon: {e}");
        Vec::new()
    });
    let mut icon_name = None;
    for icon in icons {
        let dir = hicolor.join(&icon.size).join("apps");
        let dest = dir.join(format!("{int_name}.{}", icon.ext));
        match fs::create_dir_all(&dir).and_then(|()| fs::write(&dest, &icon.data)) {
            Ok(()) => {
                eprintln!("  Icon: {}", dest.display());
                icon_name = Some(int_name.clone());
            }
            Err(e) => eprintln!("onelf-rt: failed to write icon: {e}"),
        }
    }

    // Install desktop file
    let desktop_dir = data_home.join("applications");
    if let Err(e) = fs::create_dir_all(&desktop_dir) {
        eprintln!("onelf-rt: failed to create applications dir: {e}");
        std::process::exit(1);
    }
    let dest = desktop_dir.join(format!("{int_name}.desktop"));

    let content = match desktop {
        Some(text) => patch_desktop_file(&text, exec_path, exec_entrypoint, icon_name.as_deref()),
        None => generate_desktop_file(&pkg_name, exec_path, exec_entrypoint, icon_name.as_deref()),
    };

    if let Err(e) = fs::write(&dest, content.as_bytes()) {
        eprintln!("onelf-rt: failed to write desktop file: {e}");
        std::process::exit(1);
    }
    eprintln!("  Desktop: {}", dest.display());

    update_desktop_database(&data_home);
    eprintln!("Integrated '{pkg_name}'");
}

fn do_unintegrate(pkg: &PackageData, ep_name: &str) {
    let Some(data_home) = xdg_data_home() else {
        eprintln!("onelf-rt: cannot determine XDG_DATA_HOME or HOME");
        std::process::exit(1);
    };

    let name = pkg.manifest.name();
    let pkg_name = if name.is_empty() {
        ep_name.to_string()
    } else {
        name.to_string()
    };
    let int_name = integration_name(pkg, &pkg_name, ep_name);

    let mut removed = false;

    let desktop_path = data_home
        .join("applications")
        .join(format!("{int_name}.desktop"));
    if desktop_path.exists() {
        if let Err(e) = fs::remove_file(&desktop_path) {
            eprintln!("onelf-rt: failed to remove desktop file: {e}");
        } else {
            eprintln!("  Removed: {}", desktop_path.display());
            removed = true;
        }
    }

    let hicolor = data_home.join("icons/hicolor");
    if hicolor.exists() {
        removed |= remove_icons(&hicolor, &int_name);
    }

    update_desktop_database(&data_home);

    if removed {
        eprintln!("Unintegrated '{pkg_name}'");
    } else {
        eprintln!("Nothing was installed for '{pkg_name}'");
    }
}

/// Check args for `--onelf-integrate` or `--onelf-unintegrate` and handle them.
/// Returns `true` if a flag was handled (caller should exit).
pub fn handle_integrate_flags(
    args: &[OsString],
    pkg: &mut PackageData,
    ep_name: &str,
    exec_path: &str,
) -> bool {
    let is_integrate = args.iter().any(|a| a.as_os_str() == "--onelf-integrate");
    let is_unintegrate = args.iter().any(|a| a.as_os_str() == "--onelf-unintegrate");

    if is_integrate {
        do_integrate(pkg, ep_name, exec_path);
        true
    } else if is_unintegrate {
        do_unintegrate(pkg, ep_name);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{generate_desktop_file, xdg_data_home_from};

    #[test]
    fn generated_file_quotes_exec_and_keeps_tryexec_literal() {
        let out = generate_desktop_file("App", "/opt/has space/app", None, Some("icon"));
        assert!(out.contains("Exec=\"/opt/has space/app\"\n"), "got:\n{out}");
        assert!(
            out.contains("TryExec=/opt/has space/app\n"),
            "TryExec is a literal path: {out}"
        );
        assert!(out.contains("Icon=icon\n"));
    }

    #[test]
    fn a_relative_xdg_data_home_falls_back_to_home() {
        use std::ffi::OsString;
        assert_eq!(
            xdg_data_home_from(
                Some(OsString::from("relative/path")),
                Some(OsString::from("/home/me"))
            )
            .unwrap(),
            PathBuf::from("/home/me/.local/share")
        );
        // An absolute one is honored.
        assert_eq!(
            xdg_data_home_from(
                Some(OsString::from("/abs/data")),
                Some(OsString::from("/home/me"))
            )
            .unwrap(),
            PathBuf::from("/abs/data")
        );
        // No usable XDG and no HOME: nothing to install into.
        assert!(xdg_data_home_from(Some(OsString::from("rel")), None).is_none());
    }
}
