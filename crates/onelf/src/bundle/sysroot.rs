//! Populating an AppDir from a pinned sysroot.
//!
//! The application's package, everything it depends on, and the optional
//! dependencies the recipe names are copied out of the sysroot with
//! `usr/` flattened into the conventional AppDir layout. The platform
//! line, the policy and an optional trace decide what stays out. Nothing
//! from the packer's own machine is consulted.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use onelf_sysroot::prune::prune;
use onelf_sysroot::{Database, PlatformLine, Policy, Trace};

use super::elf::parse_needed;
use super::ui::color;

/// Where the bundle's contents come from and what is left out.
#[derive(Debug, Clone, Default)]
pub struct SysrootOptions {
    /// A materialized rootfs with its package database.
    pub root: PathBuf,
    /// The entrypoint relative to the AppDir, as in the recipe.
    pub command: String,
    /// The label recorded as the package's platform in its provenance.
    pub platform: String,
    /// Optional dependencies to include, by package name.
    pub optional: Vec<String>,
    /// Sonames the host provides, one prefix per line.
    pub platform_line: Option<PathBuf>,
    /// Paths that never ship, one glob per line.
    pub policy: Option<PathBuf>,
    /// Paths a test run opened, one per line.
    pub trace: Option<PathBuf>,
}

/// What populating did, for the report.
#[derive(Debug, Default)]
pub struct SysrootReport {
    pub packages: Vec<(String, String)>,
    pub unsatisfied: Vec<(String, String)>,
    pub host_provided: Vec<String>,
    pub removed_platform: usize,
    pub removed_policy: usize,
    pub removed_trace: usize,
    pub copied: usize,
    /// Files the database lists that the sysroot does not hold.
    pub absent: usize,
}

/// Copy the closure of the entrypoint's package into `appdir`. Returns
/// the report and the platform line, which the dependency walk needs too.
pub fn populate(appdir: &Path, opts: &SysrootOptions) -> io::Result<(SysrootReport, PlatformLine)> {
    let root = &opts.root;
    // A sysroot inside the AppDir would be packed along with the bundle,
    // and its library directories would be found by every walk that
    // looks for `.so` files.
    if let (Ok(root_abs), Ok(appdir_abs)) = (root.canonicalize(), appdir.canonicalize())
        && root_abs.starts_with(&appdir_abs)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: the sysroot must not be inside the AppDir {}",
                root.display(),
                appdir.display()
            ),
        ));
    }
    let db = Database::read(root)?;
    let command = opts
        .command
        .trim_start_matches("./")
        .trim_start_matches('/');
    let candidates = [format!("usr/{command}"), command.to_string()];
    let owner = candidates.iter().find_map(|c| db.owner(c)).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{}: no package in the sysroot owns usr/{command} or {command}",
                root.display()
            ),
        )
    })?;

    let closure = db.closure(&owner.name, &opts.optional);
    let files = db.files_of(&closure);
    let platform = match &opts.platform_line {
        Some(p) => PlatformLine::load(p)?,
        None => PlatformLine::default(),
    };
    let policy = opts.policy.as_deref().map(Policy::load).transpose()?;
    let trace = opts.trace.as_deref().map(Trace::load).transpose()?;

    // Under a trace, a soname some object in the closure needs survives
    // even when the test run never mapped it.
    let mut keep: HashSet<String> = HashSet::new();
    if trace.is_some() {
        for rel in &files {
            let path = root.join(rel);
            if is_elf(&path)
                && let Ok(needed) = parse_needed(&path)
            {
                keep.extend(needed);
            }
        }
    }
    let pruned = prune(
        &files,
        Some(&platform),
        policy.as_ref(),
        trace.as_ref(),
        &keep,
    );

    let mut report = SysrootReport {
        packages: closure
            .packages
            .iter()
            .filter_map(|n| db.package(n))
            .map(|p| (p.name.clone(), p.version.clone()))
            .collect(),
        unsatisfied: closure.unsatisfied.clone(),
        host_provided: pruned.host_provided.clone(),
        removed_platform: pruned.removed_platform,
        removed_policy: pruned.removed_policy,
        removed_trace: pruned.removed_trace,
        ..Default::default()
    };
    for rel in &pruned.kept {
        match copy_entry(root, rel, appdir)? {
            true => report.copied += 1,
            false => report.absent += 1,
        }
    }
    let record = appdir.join(PROVENANCE_FILE);
    if let Some(parent) = record.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&record, render_provenance(&opts.platform, &report.packages))?;
    super::normalize_mtime(&record);
    Ok((report, platform))
}

/// Where the bundle records what it was built from, relative to the
/// AppDir. Read by `onelf info`, never by the runtime.
pub const PROVENANCE_FILE: &str = ".onelf/provenance.toml";

/// The provenance record: the platform label and every contributing
/// package with its version, in name order.
pub fn render_provenance(platform: &str, packages: &[(String, String)]) -> String {
    let mut out = format!("platform = {}\n", toml_string(platform));
    for (name, version) in packages {
        out.push_str(&format!(
            "\n[[package]]\nname = {}\nversion = {}\n",
            toml_string(name),
            toml_string(version)
        ));
    }
    out
}

/// A basic TOML string: quotes, backslashes and control characters
/// escaped, so a package name cannot open a new table.
fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The label a sysroot gets when the recipe names none: the archive's
/// file name, or the directory's.
pub fn default_label(root: &Path, archive: Option<&Path>) -> String {
    archive
        .and_then(|a| a.file_name())
        .or_else(|| root.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sysroot".to_string())
}

/// The AppDir path a sysroot path lands at: `usr/` is the prefix the
/// conventional layout leaves out.
fn appdir_path(rel: &str) -> &str {
    rel.strip_prefix("usr/").unwrap_or(rel)
}

/// Copy one file or symlink. Returns false when the sysroot lacks it,
/// which a debloated rootfs does for files its database still lists.
fn copy_entry(root: &Path, rel: &str, appdir: &Path) -> io::Result<bool> {
    let src = root.join(rel);
    let Ok(md) = fs::symlink_metadata(&src) else {
        return Ok(false);
    };
    let dest_rel = PathBuf::from(appdir_path(rel));
    let dest = appdir.join(&dest_rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if md.file_type().is_symlink() {
        let target = fs::read_link(&src)?;
        let Some(target) = relink(rel, &target) else {
            // A link that flattening folds onto itself, such as the
            // `bin -> usr/bin` compatibility links a rootfs carries.
            return Ok(false);
        };
        let target_str = target.to_string_lossy();
        if !onelf_format::symlink_target_within_root(&dest_rel, &target_str) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{rel}: symlink target {} leaves the bundle",
                    target.display()
                ),
            ));
        }
        match fs::symlink_metadata(&dest) {
            Ok(existing) if existing.is_dir() => return Ok(false),
            Ok(_) => fs::remove_file(&dest)?,
            Err(_) => {}
        }
        std::os::unix::fs::symlink(&target, &dest)?;
        return Ok(true);
    }
    if !md.file_type().is_file() {
        return Ok(false);
    }
    if fs::symlink_metadata(&dest).is_ok() {
        fs::remove_file(&dest)?;
    }
    fs::copy(&src, &dest)?;
    super::normalize_mtime(&dest);
    Ok(true)
}

/// A symlink target as it must read at the link's new location, or
/// `None` when flattening folds the link onto itself.
///
/// The target is resolved in sysroot space first, then mapped through the
/// same `usr/` flattening as the link, then made relative to the link's
/// directory. A relative target is taken relative to the link's directory
/// in the sysroot, which is what the loader would do.
fn relink(link_rel: &str, target: &Path) -> Option<PathBuf> {
    let link = Path::new(link_rel);
    let resolved = if target.is_absolute() {
        normalize(target.strip_prefix("/").unwrap_or(target))
    } else {
        normalize(&link.parent().unwrap_or(Path::new("")).join(target))
    };
    let mapped = PathBuf::from(appdir_path(&resolved.to_string_lossy()));
    let dest = PathBuf::from(appdir_path(link_rel));
    if mapped == dest {
        return None;
    }
    let from = dest.parent().unwrap_or(Path::new(""));
    Some(relative_path(from, &mapped))
}

/// `path` with `.` and `..` components folded, without touching the
/// filesystem. A `..` at the root is dropped rather than escaping.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// `to` expressed relative to the directory `from`, both relative to the
/// same root.
fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c.as_os_str());
    }
    out
}

fn is_elf(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    matches!(f.read(&mut magic), Ok(4)) && magic == *b"\x7fELF"
}

pub fn print_report(opts: &SysrootOptions, report: &SysrootReport) {
    eprintln!("{} {}", color::bold("Sysroot:"), opts.root.display());
    eprintln!(
        "  {} ({}): {}",
        color::bold("Packages"),
        report.packages.len(),
        report
            .packages
            .iter()
            .map(|(n, v)| format!("{n} {v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!(
        "  {} {} files; left out: {} on the platform line, {} by policy, {} by trace{}",
        color::bold("Copied"),
        report.copied,
        report.removed_platform,
        report.removed_policy,
        report.removed_trace,
        if report.absent > 0 {
            format!("; {} listed but absent from the sysroot", report.absent)
        } else {
            String::new()
        }
    );
    if !report.host_provided.is_empty() {
        eprintln!(
            "  {} {}",
            color::bold("Host-provided:"),
            report.host_provided.join(", ")
        );
    }
    for (dep, by) in &report.unsatisfied {
        eprintln!(
            "  {} {dep} (needed by {by}) is not installed in the sysroot",
            color::bold_red("warning:")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_targets_are_remapped_relative_to_the_link() {
        let some = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            relink("usr/lib/libfoo.so", Path::new("libfoo.so.1")),
            some("libfoo.so.1")
        );
        assert_eq!(
            relink("usr/lib/libfoo.so", Path::new("/usr/lib/libfoo.so.1")),
            some("libfoo.so.1")
        );
        assert_eq!(
            relink("usr/bin/tool", Path::new("/usr/lib/tool/bin/tool")),
            some("../lib/tool/bin/tool")
        );
        assert_eq!(
            relink("usr/bin/tool", Path::new("../lib/tool/bin/tool")),
            some("../lib/tool/bin/tool")
        );
        assert_eq!(relink("lib64", Path::new("usr/lib")), some("lib"));
        assert_eq!(relink("usr/lib64", Path::new("lib")), some("lib"));
        assert_eq!(relink("usr/sbin", Path::new("bin")), some("bin"));
        // The compatibility links a rootfs carries fold onto themselves.
        assert_eq!(relink("bin", Path::new("usr/bin")), None);
        assert_eq!(relink("lib", Path::new("usr/lib")), None);
    }

    #[test]
    fn the_record_renders_to_fixed_bytes_with_escaping() {
        let packages = vec![
            ("app".to_string(), "1.0-1".to_string()),
            ("we\"ird".to_string(), "2\\3".to_string()),
        ];
        assert_eq!(
            render_provenance("platform-1", &packages),
            "platform = \"platform-1\"\n\n[[package]]\nname = \"app\"\nversion = \"1.0-1\"\n\n[[package]]\nname = \"we\\\"ird\"\nversion = \"2\\\\3\"\n"
        );
        assert_eq!(
            default_label(
                Path::new("/x/sysroot"),
                Some(Path::new("/y/platform-1.tar.zst"))
            ),
            "platform-1.tar.zst"
        );
        assert_eq!(default_label(Path::new("/x/sysroot"), None), "sysroot");
    }

    #[test]
    fn usr_is_flattened_and_nothing_else_is() {
        assert_eq!(appdir_path("usr/bin/app"), "bin/app");
        assert_eq!(appdir_path("usr/share/x"), "share/x");
        assert_eq!(appdir_path("etc/app.conf"), "etc/app.conf");
        assert_eq!(appdir_path("opt/app/run"), "opt/app/run");
    }
}
