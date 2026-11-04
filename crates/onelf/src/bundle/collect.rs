//! Descriptor plugin-group collection over the dependency walk.
//!
//! An activated rule's plugin group declares ordered source directories
//! against the libraries the walk has admitted. Discovery enumerates the
//! alternatives that exist and are admitted; collection walks every
//! directory a group's discovery found and hands each compatible plugin
//! to the walk as an object, written at the group's declared
//! destination with the source layout preserved.
//!
//! A member that is not an ELF object of the bundle's own class and
//! machine is refused with a reason and never shadows a compatible one,
//! so a multiarch plugin directory contributes only its matching half.
//! Collection is bounded: a directory past the member or depth cap is a
//! refused group, not an unbounded walk.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use onelf_format::rules::{Requirement, ResourceGroup};

use super::{
    AdmittedLibrary, AdmittedUniverse, CandidateHit, GroupQuery, discover_group, is_nix_stub_ld,
    miss_text, read_elf_class, read_elf_machine, resolve_within,
};
use crate::bundle::sysroot::SysrootUniverse;

/// The most members one group's collection may yield. A directory past
/// this is a hostile or pathological source, not completeness.
const MAX_GROUP_MEMBERS: usize = 4096;

/// The deepest one found directory is walked for members.
const MAX_GROUP_DEPTH: usize = 16;

/// The most unavailable locations one report names, so a wide
/// declaration cannot flood the build log.
const SHOWN_MISSES: usize = 8;

/// One compatible plugin a group collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollectedPlugin {
    /// The file name the plugin answers to, which the walk resolves and
    /// the loader asks for.
    pub soname: String,
    /// The destination relative to the AppDir, layout preserved.
    pub destination: PathBuf,
    /// Where the member was found, so the walk can search beside it
    /// when the plugin sits in a subdirectory of the group location.
    pub resolved: PathBuf,
}

/// What one activated plugin group's collection contributes to the walk.
#[derive(Debug, Default)]
pub(crate) struct GroupCollection {
    /// The rule that owns the group.
    pub rule: String,
    /// The group's identity within its rule.
    pub group: String,
    /// The directories discovery found, which the walk searches for the
    /// collected sonames.
    pub search_dirs: Vec<PathBuf>,
    /// The compatible plugins, in discovery order.
    pub plugins: Vec<CollectedPlugin>,
    /// Members examined and refused, each with its reason.
    pub skipped: Vec<(String, String)>,
    /// Why each declared location held nothing usable, bounded.
    pub misses: Vec<String>,
}

/// Collect one activated plugin group over the libraries the walk
/// admitted. A group whose declaration is required and that yields no
/// usable plugin is an error naming the rule, the group and the
/// reasons; an optional one is reported and the walk goes on.
pub(crate) fn collect_group(
    rule: &str,
    declared: &ResourceGroup,
    admitted: &[AdmittedLibrary],
    sysroot: Option<&SysrootUniverse>,
    target_class: Option<u8>,
    target_machine: Option<u16>,
) -> io::Result<GroupCollection> {
    let outcome = discover_group(
        &GroupQuery {
            rule,
            group: declared.id.as_str(),
            kind: declared.kind,
            destination: &declared.destination,
            candidates: &declared.candidates,
        },
        &AdmittedUniverse {
            libraries: admitted,
            sysroot,
        },
    )?;

    let mut collection = GroupCollection {
        rule: rule.to_string(),
        group: declared.id.as_str().to_string(),
        search_dirs: Vec::new(),
        plugins: Vec::new(),
        skipped: Vec::new(),
        misses: Vec::new(),
    };
    // Members are keyed by where they land: a nested layout keeps
    // same-named members apart (babl's `x86-64-v2/` and `x86-64-v3/`
    // variants), and only a second member for one destination is
    // refused, with the member that took it named.
    let mut taken: HashMap<PathBuf, PathBuf> = HashMap::new();
    for hit in &outcome.found {
        collection.search_dirs.push(hit.resolved.clone());
        for (rel, path) in members(hit, sysroot)? {
            let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".so") {
                continue;
            }
            let shown = rel.to_string_lossy().into_owned();
            if let Err(reason) = compatible_object(&path, target_class, target_machine) {
                collection.skipped.push((shown, reason));
                continue;
            }
            let destination = hit.destination.join(&rel);
            if let Some(first) = taken.get(&destination) {
                collection.skipped.push((
                    shown,
                    format!(
                        "{} is already collected from {}",
                        destination.display(),
                        first.display()
                    ),
                ));
                continue;
            }
            if collection.plugins.len() >= MAX_GROUP_MEMBERS {
                return Err(bounded(rule, declared.id.as_str(), MAX_GROUP_MEMBERS));
            }
            taken.insert(destination.clone(), path.clone());
            collection.plugins.push(CollectedPlugin {
                soname: name.to_string(),
                destination,
                resolved: path.clone(),
            });
        }
    }
    collection.misses = outcome
        .misses
        .iter()
        .take(SHOWN_MISSES)
        .map(|m| format!("{}: {}", m.root.display(), miss_text(&m.reason)))
        .collect();

    if collection.plugins.is_empty() && declared.requirement == Requirement::Required {
        let mut text = format!(
            "rule {rule} group {} is required but no usable plugin was found",
            declared.id.as_str()
        );
        for reason in collection.misses.iter().take(3) {
            text.push_str("; ");
            text.push_str(reason);
        }
        for (name, reason) in collection.skipped.iter().take(3) {
            text.push_str(&format!("; {name}: {reason}"));
        }
        return Err(io::Error::new(io::ErrorKind::NotFound, text));
    }
    Ok(collection)
}

/// The bounded-collection error: which group stopped, and at what.
fn bounded(rule: &str, group: &str, cap: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "rule {rule} group {group}: more than {cap} members in one plugin \
             group; refusing unbounded collection"
        ),
    )
}

/// The files one found directory holds, as pairs of the path relative
/// to the directory and the absolute path, in sorted order. A sysroot
/// build takes its members from the build's own admitted files, so a
/// pruned member is never walked; a host scan walks the directory.
fn members(
    hit: &CandidateHit,
    sysroot: Option<&SysrootUniverse>,
) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    if let Some(universe) = sysroot {
        let dir_rel = hit
            .resolved
            .strip_prefix(universe.root())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}: outside the sysroot it was found in",
                        hit.resolved.display()
                    ),
                )
            })?
            .to_string_lossy()
            .into_owned();
        // Each admitted member, a link among them, resolves inside the
        // sysroot, never through an absolute link onto the packer's
        // machine.
        return Ok(universe
            .admitted_under(&dir_rel)
            .into_iter()
            .filter_map(|f| {
                let rel = PathBuf::from(&f)
                    .strip_prefix(&dir_rel)
                    .unwrap_or(Path::new(&f))
                    .to_path_buf();
                let path = onelf_sysroot::path::in_root(universe.root(), Path::new(&f)).ok()?;
                Some((rel, path))
            })
            .collect());
    }
    let mut out = Vec::new();
    walk_members(&hit.root, &hit.resolved, Path::new(""), 0, &mut out)?;
    Ok(out)
}

/// The regular-file members under `dir`, as pairs of the path relative
/// to the group directory and the file to read. A symlinked member, an
/// ALSA module alias say, is followed with every landing confined to
/// `root`, the admitted root the group was found under, so the alias
/// is collected under its own name and a link leaving the root is not.
fn walk_members(
    root: &Path,
    dir: &Path,
    rel: &Path,
    depth: usize,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<()> {
    if depth > MAX_GROUP_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: deeper than the {MAX_GROUP_DEPTH} level group walk limit",
                dir.display()
            ),
        ));
    }
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let child_rel = rel.join(entry.file_name());
        let child = dir.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk_members(root, &child, &child_rel, depth + 1, out)?;
        } else if kind.is_file() {
            out.push((child_rel, child));
        } else if kind.is_symlink()
            && let Ok(inside) = child.strip_prefix(root)
            && let Ok(target) = resolve_within(root, inside)
            && target.is_file()
        {
            out.push((child_rel, target));
        }
    }
    Ok(())
}

/// Whether a member is an ELF object the bundle can load: the same
/// checks the library walk applies to every candidate it resolves. A
/// non-object, a wrong class and another machine are all refusals, so
/// an incompatible candidate is recorded instead of shadowing a valid
/// one.
fn compatible_object(
    path: &Path,
    target_class: Option<u8>,
    target_machine: Option<u16>,
) -> Result<(), String> {
    if is_nix_stub_ld(path) {
        return Err("a stub loader that loads nothing".to_string());
    }
    let Some(class) = read_elf_class(path) else {
        return Err("not an ELF object".to_string());
    };
    if let Some(want) = target_class
        && class != want
    {
        return Err(format!(
            "ELF class {class}, the bundle builds for class {want}"
        ));
    }
    match (target_machine, read_elf_machine(path)) {
        (Some(want), Some(machine)) if machine != want => Err(format!(
            "machine {machine}, the bundle builds for machine {want}"
        )),
        (Some(_), None) => Err("not an ELF object".to_string()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::sysroot::fixture;
    use onelf_format::catalog;

    /// Minimal ELF-shaped bytes: magic + EI_CLASS + e_machine, enough
    /// for the compatibility checks to classify.
    fn fake_elf(class: u8, machine: u16) -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = class;
        v[18..20].copy_from_slice(&machine.to_le_bytes());
        v
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn admitted_lib(path: &Path) -> AdmittedLibrary {
        AdmittedLibrary::new(path.file_name().unwrap().to_str().unwrap(), path)
    }

    fn builtin_group(rule: &str) -> ResourceGroup {
        catalog::builtins()
            .iter()
            .find(|d| d.id.as_str() == rule)
            .unwrap()
            .resources
            .plugins
            .first()
            .unwrap()
            .clone()
    }

    fn collected<'a>(collection: &'a GroupCollection, soname: &str) -> &'a CollectedPlugin {
        collection
            .plugins
            .iter()
            .find(|p| p.soname == soname)
            .unwrap()
    }

    #[test]
    fn an_activated_group_collects_every_compatible_member() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let lib = dir.join("usr/lib");
        write(&lib.join("libasound.so.2"), &fake_elf(2, 62));
        write(&lib.join("alsa-lib/pulse.so"), &fake_elf(2, 62));
        write(&lib.join("alsa-lib/loop.so"), &fake_elf(2, 62));
        write(&lib.join("alsa-lib/README"), b"notes");
        let libs = [admitted_lib(&lib.join("libasound.so.2"))];

        let collection = collect_group(
            "alsa",
            &builtin_group("alsa"),
            &libs,
            None,
            Some(2),
            Some(62),
        )
        .unwrap();

        assert_eq!(collection.search_dirs, vec![lib.join("alsa-lib")]);
        assert_eq!(collection.plugins.len(), 2, "{:?}", collection.plugins);
        assert_eq!(
            collected(&collection, "pulse.so").destination,
            PathBuf::from("lib/alsa-lib/pulse.so")
        );
        assert!(collection.skipped.is_empty(), "{:?}", collection.skipped);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Same-named members in different subdirectories of one group are
    /// different plugins: each lands at its own destination, copied
    /// from where it was found, and none is dropped as a duplicate.
    #[test]
    fn same_named_members_in_nested_layouts_are_all_collected() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let lib = dir.join("usr/lib");
        write(&lib.join("libbabl-0.1.so.0"), &fake_elf(2, 62));
        write(&lib.join("babl-0.1/x86-64-v2/cmyk.so"), &fake_elf(2, 62));
        write(&lib.join("babl-0.1/x86-64-v3/cmyk.so"), &fake_elf(2, 62));
        write(&lib.join("babl-0.1/cmyk.so"), &fake_elf(2, 62));
        let libs = [admitted_lib(&lib.join("libbabl-0.1.so.0"))];

        let collection = collect_group(
            "babl",
            &builtin_group("babl"),
            &libs,
            None,
            Some(2),
            Some(62),
        )
        .unwrap();
        let mut found: Vec<(PathBuf, PathBuf)> = collection
            .plugins
            .iter()
            .map(|p| (p.destination.clone(), p.resolved.clone()))
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                (
                    PathBuf::from("lib/babl-0.1/cmyk.so"),
                    lib.join("babl-0.1/cmyk.so")
                ),
                (
                    PathBuf::from("lib/babl-0.1/x86-64-v2/cmyk.so"),
                    lib.join("babl-0.1/x86-64-v2/cmyk.so")
                ),
                (
                    PathBuf::from("lib/babl-0.1/x86-64-v3/cmyk.so"),
                    lib.join("babl-0.1/x86-64-v3/cmyk.so")
                ),
            ]
        );
        assert!(collection.skipped.is_empty(), "{:?}", collection.skipped);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A symlinked member, an ALSA module alias, is collected under its
    /// own name from the file it names inside the admitted root, and a
    /// link that leaves the root is not followed.
    #[test]
    fn a_symlinked_member_is_followed_inside_its_root() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let lib = dir.join("usr/lib");
        let outside = dir.join("outside/libasound_module_pcm_far.so");
        write(&lib.join("libasound.so.2"), &fake_elf(2, 62));
        write(
            &lib.join("alsa-lib/libasound_module_pcm_pulse.so"),
            &fake_elf(2, 62),
        );
        write(&outside, &fake_elf(2, 62));
        std::os::unix::fs::symlink(
            "libasound_module_pcm_pulse.so",
            lib.join("alsa-lib/libasound_module_pcm_alias.so"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, lib.join("alsa-lib/libasound_module_pcm_far.so"))
            .unwrap();
        let libs = [admitted_lib(&lib.join("libasound.so.2"))];

        let collection = collect_group(
            "alsa",
            &builtin_group("alsa"),
            &libs,
            None,
            Some(2),
            Some(62),
        )
        .unwrap();
        let alias = collected(&collection, "libasound_module_pcm_alias.so");
        assert_eq!(
            alias.destination,
            PathBuf::from("lib/alsa-lib/libasound_module_pcm_alias.so")
        );
        assert_eq!(
            alias.resolved,
            lib.join("alsa-lib/libasound_module_pcm_pulse.so")
        );
        assert!(
            !collection
                .plugins
                .iter()
                .any(|p| p.soname == "libasound_module_pcm_far.so"),
            "{:?}",
            collection.plugins
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nested_layout_is_preserved_into_the_destination() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let lib = dir.join("usr/lib/x86_64-linux-gnu");
        write(&lib.join("libheif.so.1"), &fake_elf(2, 62));
        write(&lib.join("libheif/plugins/x264.so"), &fake_elf(2, 62));
        write(&lib.join("libheif/deep/a/avif.so"), &fake_elf(2, 62));
        let libs = [admitted_lib(&lib.join("libheif.so.1"))];

        let collection = collect_group(
            "heif",
            &builtin_group("heif"),
            &libs,
            None,
            Some(2),
            Some(62),
        )
        .unwrap();

        assert_eq!(
            collected(&collection, "x264.so").destination,
            PathBuf::from("lib/libheif/plugins/x264.so"),
            "the nested layout a distribution ships is kept"
        );
        assert_eq!(
            collected(&collection, "avif.so").destination,
            PathBuf::from("lib/libheif/deep/a/avif.so")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_incompatible_member_is_refused_without_shadowing() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let lib = dir.join("usr/lib");
        write(&lib.join("libgstreamer-1.0.so.0"), &fake_elf(2, 62));
        write(&lib.join("gstreamer-1.0/arm.so"), &fake_elf(2, 183));
        write(&lib.join("gstreamer-1.0/text.so"), b"not elf");
        write(&lib.join("gstreamer-1.0/thin.so"), &fake_elf(1, 62));
        write(&lib.join("gstreamer-1.0/core.so"), &fake_elf(2, 62));
        write(&lib.join("gstreamer-1.0/helper"), b"not a candidate");
        let libs = [admitted_lib(&lib.join("libgstreamer-1.0.so.0"))];

        let collection = collect_group(
            "gstreamer",
            &builtin_group("gstreamer"),
            &libs,
            None,
            Some(2),
            Some(62),
        )
        .unwrap();

        assert_eq!(collection.plugins.len(), 1);
        assert_eq!(collection.plugins[0].soname, "core.so");
        let refused: Vec<(&str, &str)> = collection
            .skipped
            .iter()
            .map(|(n, r)| (n.as_str(), r.as_str()))
            .collect();
        assert_eq!(
            refused,
            [
                ("arm.so", "machine 183, the bundle builds for machine 62"),
                ("text.so", "not an ELF object"),
                ("thin.so", "ELF class 1, the bundle builds for class 2"),
            ],
            "every refusal names its reason, in walk order"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sysroot_group_walks_only_admitted_members() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("sys");
        write(&root.join("usr/bin/app"), &fake_elf(2, 62));
        write(&root.join("usr/lib/libgio-2.0.so.0"), &fake_elf(2, 62));
        write(
            &root.join("usr/lib/gio/modules/gnutls.so"),
            &fake_elf(2, 62),
        );
        write(&root.join("usr/lib/gio/modules/other.so"), &fake_elf(2, 62));
        let mut app = fixture::package("app", &["usr/bin/app", "usr/lib/libgio-2.0.so.0"]);
        app.depends = vec!["gio".into()];
        let mut gio = fixture::package(
            "gio",
            &["usr/lib/libgio-2.0.so.0", "usr/lib/gio/modules/gnutls.so"],
        );
        gio.provides = vec!["gio".into()];
        let extra = fixture::package("extra-modules", &["usr/lib/gio/modules/other.so"]);
        let universe =
            fixture::populate_universe(&root, &[app, gio, extra], "app", &[], None, None);
        let libs = [admitted_lib(&root.join("usr/lib/libgio-2.0.so.0"))];

        let collection = collect_group(
            "gio",
            &builtin_group("gio"),
            &libs,
            Some(&universe),
            Some(2),
            Some(62),
        )
        .unwrap();

        assert_eq!(collection.plugins.len(), 1, "{:?}", collection.plugins);
        assert_eq!(collection.plugins[0].soname, "gnutls.so");
        assert_eq!(
            collection.plugins[0].destination,
            PathBuf::from("lib/gio/modules/gnutls.so"),
            "the unselected package's member never becomes a candidate"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_required_group_with_nothing_usable_is_an_error() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let lib = dir.join("usr/lib");
        write(&lib.join("libasound.so.2"), &fake_elf(2, 62));
        write(&lib.join("alsa-lib/arm.so"), &fake_elf(2, 183));
        let libs = [admitted_lib(&lib.join("libasound.so.2"))];
        let mut required = builtin_group("alsa");
        required.requirement = Requirement::Required;

        let err = collect_group("alsa", &required, &libs, None, Some(2), Some(62))
            .expect_err("a required group with only an incompatible member fails");
        assert!(
            err.to_string()
                .contains("rule alsa group plugins is required but no usable plugin"),
            "{err}"
        );
        assert!(
            err.to_string().contains("arm.so"),
            "the refusal names the incompatible member: {err}"
        );

        fs::remove_dir_all(lib.join("alsa-lib")).unwrap();
        let err = collect_group("alsa", &required, &libs, None, Some(2), Some(62))
            .expect_err("a required group with no directory fails");
        assert!(
            err.to_string().contains("absent"),
            "the refusal names the absent location: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_optional_group_with_nothing_usable_is_reported_not_failed() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let lib = dir.join("usr/lib");
        write(&lib.join("libbabl-0.1.so.0"), &fake_elf(2, 62));
        let libs = [admitted_lib(&lib.join("libbabl-0.1.so.0"))];

        let collection = collect_group(
            "babl",
            &builtin_group("babl"),
            &libs,
            None,
            Some(2),
            Some(62),
        )
        .unwrap();

        assert!(collection.plugins.is_empty());
        assert_eq!(collection.misses.len(), 1, "{:?}", collection.misses);
        assert!(collection.misses[0].contains("absent"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_member_in_two_roots_is_collected_once() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let one = dir.join("one/usr/lib");
        let two = dir.join("two/usr/lib");
        write(&one.join("libgio-2.0.so.0"), &fake_elf(2, 62));
        write(&two.join("libgio-2.0.so.0"), &fake_elf(2, 62));
        write(&one.join("gio/modules/shared.so"), &fake_elf(2, 62));
        write(&two.join("gio/modules/shared.so"), &fake_elf(2, 62));
        write(&two.join("gio/modules/other.so"), &fake_elf(2, 62));
        let libs = [
            admitted_lib(&one.join("libgio-2.0.so.0")),
            admitted_lib(&two.join("libgio-2.0.so.0")),
        ];

        let collection =
            collect_group("gio", &builtin_group("gio"), &libs, None, Some(2), Some(62)).unwrap();

        assert_eq!(collection.search_dirs.len(), 2);
        assert_eq!(collection.plugins.len(), 2, "{:?}", collection.plugins);
        assert_eq!(
            collected(&collection, "shared.so").destination,
            PathBuf::from("lib/gio/modules/shared.so"),
            "the first root in admission order decides the soname"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collection_is_bounded() {
        let dir =
            std::env::temp_dir().join(format!("onelf-gather-{}-{}", std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        let lib = dir.join("usr/lib");
        write(&lib.join("libpipewire-0.3.so.0"), &fake_elf(2, 62));
        for i in 0..MAX_GROUP_MEMBERS + 1 {
            write(&lib.join(format!("pipewire-0.3/m{i}.so")), &fake_elf(2, 62));
        }
        let libs = [admitted_lib(&lib.join("libpipewire-0.3.so.0"))];

        let err = collect_group(
            "pipewire",
            &builtin_group("pipewire"),
            &libs,
            None,
            Some(2),
            Some(62),
        )
        .expect_err("a group past the member cap is refused");
        assert!(
            err.to_string()
                .contains("more than 4096 members in one plugin group"),
            "{err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
