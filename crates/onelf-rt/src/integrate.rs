//! XDG desktop integration from a running onelf binary.
//!
//! Handles `--onelf-integrate` and `--onelf-unintegrate` flags to install or
//! remove .desktop files and icons without needing the `onelf` CLI tool.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::loader::PackageData;
use crate::metadata::{resolve_desktop, resolve_icon};

fn read_entry(pkg: &mut PackageData, entry_idx: usize) -> io::Result<Vec<u8>> {
    let blocks = pkg.manifest.entries[entry_idx].blocks.clone();
    crate::loader::read_payload_blocks(
        &mut pkg.file,
        pkg.footer.payload_offset,
        &blocks,
        pkg.dict.as_deref(),
        pkg.footer.is_stored(),
    )
}

fn xdg_data_home() -> Option<PathBuf> {
    if let Some(val) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(val));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"))
}

fn integration_name(pkg_name: &str) -> String {
    let sanitized: String = pkg_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("onelf-{sanitized}")
}

fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || data[0..8] != *b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

fn remove_icons(hicolor: &Path, int_name: &str) {
    let entries = match fs::read_dir(hicolor) {
        Ok(e) => e,
        Err(_) => return,
    };
    let target_svg = format!("{int_name}.svg");
    let target_png = format!("{int_name}.png");

    for entry in entries.flatten() {
        if !entry.file_type().map_or(false, |ft| ft.is_dir()) {
            continue;
        }
        let apps_dir = entry.path().join("apps");
        if !apps_dir.is_dir() {
            continue;
        }
        for name in [&target_svg, &target_png] {
            let path = apps_dir.join(name);
            if path.exists() {
                let _ = fs::remove_file(&path);
                eprintln!("  Removed: {}", path.display());
            }
        }
    }
}

fn patch_desktop_file(content: &str, exec_path: &str, icon_name: Option<&str>) -> String {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let mut has_exec = false;
    let mut has_tryexec = false;

    for line in &mut lines {
        if line.starts_with("Exec=") {
            let rest = &line[5..];
            let mut parts = rest.split_whitespace();
            let _old = parts.next();
            let tail: Vec<&str> = parts.collect();
            if tail.is_empty() {
                *line = format!("Exec={exec_path}");
            } else {
                *line = format!("Exec={exec_path} {}", tail.join(" "));
            }
            has_exec = true;
        } else if line.starts_with("TryExec=") {
            *line = format!("TryExec={exec_path}");
            has_tryexec = true;
        } else if line.starts_with("Icon=") {
            if let Some(name) = icon_name {
                *line = format!("Icon={name}");
            }
        }
    }

    if !has_exec {
        lines.push(format!("Exec={exec_path}"));
    }
    if !has_tryexec {
        lines.push(format!("TryExec={exec_path}"));
    }

    let mut result = lines.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

fn generate_desktop_file(name: &str, exec_path: &str, icon_name: Option<&str>) -> String {
    let mut lines = vec![
        "[Desktop Entry]".to_string(),
        "Type=Application".to_string(),
        format!("Name={name}"),
        format!("Exec={exec_path}"),
        format!("TryExec={exec_path}"),
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
    let int_name = integration_name(&pkg_name);

    // Clean stale icons first
    let hicolor = data_home.join("icons/hicolor");
    if hicolor.exists() {
        remove_icons(&hicolor, &int_name);
    }

    // Install icon
    let icon_name = if let Some(icon_idx) = resolve_icon(&pkg.manifest, ep_name) {
        match read_entry(pkg, icon_idx) {
            Ok(icon_data) => {
                let icon_path = pkg.manifest.entry_path(icon_idx);
                let is_svg = icon_path.ends_with(".svg");

                let dest = if is_svg {
                    let dir = data_home.join("icons/hicolor/scalable/apps");
                    let _ = fs::create_dir_all(&dir);
                    dir.join(format!("{int_name}.svg"))
                } else {
                    let (w, h) = png_dimensions(&icon_data).unwrap_or((256, 256));
                    let dir = data_home.join(format!("icons/hicolor/{w}x{h}/apps"));
                    let _ = fs::create_dir_all(&dir);
                    dir.join(format!("{int_name}.png"))
                };

                if let Err(e) = fs::write(&dest, &icon_data) {
                    eprintln!("onelf-rt: failed to write icon: {e}");
                } else {
                    eprintln!("  Icon: {}", dest.display());
                }
                Some(int_name.clone())
            }
            Err(e) => {
                eprintln!("onelf-rt: failed to read icon: {e}");
                None
            }
        }
    } else {
        None
    };

    // Install desktop file
    let desktop_dir = data_home.join("applications");
    if let Err(e) = fs::create_dir_all(&desktop_dir) {
        eprintln!("onelf-rt: failed to create applications dir: {e}");
        std::process::exit(1);
    }
    let dest = desktop_dir.join(format!("{int_name}.desktop"));

    let content = if let Some(desktop_idx) = resolve_desktop(&pkg.manifest, ep_name) {
        match read_entry(pkg, desktop_idx) {
            Ok(raw) => {
                let text = String::from_utf8(raw).unwrap_or_default();
                patch_desktop_file(&text, exec_path, icon_name.as_deref())
            }
            Err(_) => generate_desktop_file(&pkg_name, exec_path, icon_name.as_deref()),
        }
    } else {
        generate_desktop_file(&pkg_name, exec_path, icon_name.as_deref())
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
    let int_name = integration_name(&pkg_name);

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
        remove_icons(&hicolor, &int_name);
        removed = true;
    }

    update_desktop_database(&data_home);

    if removed {
        eprintln!("Unintegrated '{pkg_name}'");
    } else {
        eprintln!("Nothing to unintegrate for '{pkg_name}'");
    }
}

/// Check args for `--onelf-integrate` or `--onelf-unintegrate` and handle them.
/// Returns `true` if a flag was handled (caller should exit).
pub fn handle_integrate_flags(
    args: &[String],
    pkg: &mut PackageData,
    ep_name: &str,
    exec_path: &str,
) -> bool {
    let is_integrate = args.iter().any(|a| a == "--onelf-integrate");
    let is_unintegrate = args.iter().any(|a| a == "--onelf-unintegrate");

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
