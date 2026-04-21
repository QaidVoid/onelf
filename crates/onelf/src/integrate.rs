//! XDG desktop integration: install/remove .desktop files and icons.
//!
//! `integrate` extracts the icon and desktop file from a packed binary,
//! installs them to XDG-compliant paths, and patches the desktop file's
//! `Exec=` and `Icon=` fields. `unintegrate` reverses the process.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use onelf_format::{Footer, Manifest};

use crate::extract::decompress_entry;
use crate::info::read_footer_and_manifest;
use crate::metadata::{resolve_desktop, resolve_icon};

struct IntegrationContext {
    footer: Footer,
    manifest: Manifest,
    binary: PathBuf,
    pkg_name: String,
    ep_name: String,
}

impl IntegrationContext {
    fn load(binary: &Path, entrypoint: Option<&str>) -> io::Result<Self> {
        let binary = binary.canonicalize()?;
        let (footer, manifest) = read_footer_and_manifest(&binary)?;

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
                let idx = manifest.header.default_entrypoint as usize;
                let ep = &manifest.entrypoints[idx];
                manifest.get_string(ep.name).to_string()
            }
        };

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
        })
    }

    fn read_entry(&self, entry_idx: usize) -> io::Result<Vec<u8>> {
        let entry = &self.manifest.entries[entry_idx];
        let mut file = fs::File::open(&self.binary)?;

        let dict = if self.footer.dict_size > 0 {
            file.seek(SeekFrom::Start(self.footer.dict_offset))?;
            let mut buf = vec![0u8; self.footer.dict_size as usize];
            Read::read_exact(&mut file, &mut buf)?;
            Some(buf)
        } else {
            None
        };

        decompress_entry(&mut file, &self.footer, entry, dict.as_deref())
    }
}

fn xdg_data_home() -> io::Result<PathBuf> {
    if let Some(val) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(val));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local/share"));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "neither $XDG_DATA_HOME nor $HOME is set",
    ))
}

/// Sanitize package name into a safe desktop entry name: `onelf-{name}`.
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

/// Read width and height from a PNG IHDR chunk.
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || data[0..8] != *b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

/// Install the icon and return the icon theme name for the `Icon=` field.
fn install_icon(
    ctx: &IntegrationContext,
    data_home: &Path,
    int_name: &str,
) -> io::Result<Option<String>> {
    let icon_idx = match resolve_icon(&ctx.manifest, &ctx.ep_name) {
        Some(idx) => idx,
        None => return Ok(None),
    };

    let icon_data = ctx.read_entry(icon_idx)?;
    let icon_path = ctx.manifest.entry_path(icon_idx);
    let is_svg = icon_path.ends_with(".svg");

    let dest = if is_svg {
        let dir = data_home.join("icons/hicolor/scalable/apps");
        fs::create_dir_all(&dir)?;
        dir.join(format!("{int_name}.svg"))
    } else {
        let (w, h) = png_dimensions(&icon_data).unwrap_or((256, 256));
        let dir = data_home.join(format!("icons/hicolor/{w}x{h}/apps"));
        fs::create_dir_all(&dir)?;
        dir.join(format!("{int_name}.png"))
    };

    fs::write(&dest, &icon_data)?;
    eprintln!("  Icon: {}", dest.display());

    Ok(Some(int_name.to_string()))
}

/// Install the desktop file, patching or generating as needed.
fn install_desktop(
    ctx: &IntegrationContext,
    data_home: &Path,
    int_name: &str,
    icon_name: Option<&str>,
) -> io::Result<()> {
    let desktop_dir = data_home.join("applications");
    fs::create_dir_all(&desktop_dir)?;
    let dest = desktop_dir.join(format!("{int_name}.desktop"));

    let exec_path = ctx.binary.to_str().unwrap_or("");

    let content = match resolve_desktop(&ctx.manifest, &ctx.ep_name) {
        Some(idx) => {
            let raw = ctx.read_entry(idx)?;
            let text = String::from_utf8(raw).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "desktop file is not valid UTF-8",
                )
            })?;
            patch_desktop_file(&text, exec_path, icon_name)
        }
        None => generate_desktop_file(&ctx.pkg_name, exec_path, icon_name),
    };

    fs::write(&dest, content.as_bytes())?;
    eprintln!("  Desktop: {}", dest.display());
    Ok(())
}

/// Patch `Exec=`, `TryExec=`, and `Icon=` in an existing desktop file.
fn patch_desktop_file(content: &str, exec_path: &str, icon_name: Option<&str>) -> String {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let mut has_exec = false;
    let mut has_tryexec = false;

    for line in &mut lines {
        if line.starts_with("Exec=") {
            // Replace the executable path, keep trailing arguments/field codes
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

/// Generate a minimal `.desktop` file.
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
    let int_name = integration_name(&ctx.pkg_name);

    // Remove any stale icons first (handles format changes on re-integrate)
    let hicolor = data_home.join("icons/hicolor");
    if hicolor.exists() {
        let _ = remove_icons(&hicolor, &int_name);
    }

    let icon_name = install_icon(&ctx, &data_home, &int_name)?;
    install_desktop(&ctx, &data_home, &int_name, icon_name.as_deref())?;
    update_desktop_database(&data_home);

    eprintln!("Integrated '{}'", ctx.pkg_name);
    Ok(())
}

pub fn unintegrate(binary: &Path, entrypoint: Option<&str>) -> io::Result<()> {
    let ctx = IntegrationContext::load(binary, entrypoint)?;
    let data_home = xdg_data_home()?;
    let int_name = integration_name(&ctx.pkg_name);

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
        eprintln!("Nothing to unintegrate for '{}'", ctx.pkg_name);
    }

    Ok(())
}
