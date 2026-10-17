//! A warning for the plugin and data directories bundled libraries have
//! compiled in that the bundle does not answer for.
//!
//! A library that loads plugins or reads data from an absolute path built
//! into it reads the host's copy, or finds nothing, unless the runtime
//! points it elsewhere. The runtime does that through a variable, and only
//! when the bundle carries the directory. The components are named here
//! one by one, so nothing outside the list is reported, and each names
//! where the runtime looks (see `onelf-rt`'s `env.rs`).

use std::fs;
use std::path::{Path, PathBuf};

use super::color;

/// Where the bundle carries a directory for the runtime to find it.
#[derive(Clone, Copy)]
enum Place {
    /// Below one of the bundle's library directories.
    Lib(&'static str),
    /// Below the bundle's root.
    Root(&'static str),
}

use Place::{Lib, Root};

/// A compiled-in directory, by how the path a library holds ends.
struct Component {
    ends_with: &'static str,
    places: &'static [Place],
    var: &'static str,
}

const SCANNER: &[Place] = &[
    Lib("gstreamer-1.0/gst-plugin-scanner"),
    Lib("gstreamer1.0/gstreamer-1.0/gst-plugin-scanner"),
    Root("libexec/gstreamer-1.0/gst-plugin-scanner"),
];
const MAGIC: &[Place] = &[
    Root("share/file/misc/magic.mgc"),
    Root("share/misc/magic.mgc"),
];

/// The components the lint knows. The driver stack is left out, since it
/// comes from the host by design, and so are directories the runtime adds
/// to rather than replaces (GIO modules, terminfo, locales), whose host
/// copy is meant to stay reachable. GLib, GTK, gdk-pixbuf and
/// gobject-introspection join their directories at run time, so no whole
/// path is there to find.
const COMPONENTS: &[Component] = &[
    Component {
        ends_with: "/gstreamer-1.0",
        places: &[Lib("gstreamer-1.0")],
        var: "GST_PLUGIN_SYSTEM_PATH_1_0",
    },
    Component {
        ends_with: "/gst-plugin-scanner",
        places: SCANNER,
        var: "GST_PLUGIN_SCANNER",
    },
    Component {
        ends_with: "/pipewire-0.3",
        places: &[Lib("pipewire-0.3")],
        var: "PIPEWIRE_MODULE_DIR",
    },
    Component {
        ends_with: "/spa-0.2",
        places: &[Lib("spa-0.2")],
        var: "SPA_PLUGIN_DIR",
    },
    Component {
        ends_with: "/alsa-lib",
        places: &[Lib("alsa-lib")],
        var: "ALSA_PLUGIN_DIR",
    },
    Component {
        ends_with: "/share/alsa",
        places: &[Root("share/alsa")],
        var: "ALSA_CONFIG_DIR",
    },
    Component {
        ends_with: "/libheif/plugins",
        places: &[Lib("libheif")],
        var: "LIBHEIF_PLUGIN_PATH",
    },
    Component {
        ends_with: "/libheif",
        places: &[Lib("libheif")],
        var: "LIBHEIF_PLUGIN_PATH",
    },
    Component {
        ends_with: "/babl-0.1",
        places: &[Lib("babl-0.1")],
        var: "BABL_PATH",
    },
    Component {
        ends_with: "/gegl-0.4",
        places: &[Lib("gegl-0.4")],
        var: "GEGL_PATH",
    },
    Component {
        ends_with: "/tcl8.6",
        places: &[Lib("tcl8.6"), Root("share/tcltk/tcl8.6")],
        var: "TCL_LIBRARY",
    },
    Component {
        ends_with: "/tcl9.0",
        places: &[Lib("tcl9.0"), Root("share/tcltk/tcl9.0")],
        var: "TCL_LIBRARY",
    },
    Component {
        ends_with: "/tk8.6",
        places: &[Lib("tk8.6"), Root("share/tcltk/tk8.6")],
        var: "TK_LIBRARY",
    },
    Component {
        ends_with: "/tk9.0",
        places: &[Lib("tk9.0"), Root("share/tcltk/tk9.0")],
        var: "TK_LIBRARY",
    },
    Component {
        ends_with: "/share/X11/locale",
        places: &[Root("share/X11/locale")],
        var: "XLOCALEDIR",
    },
    Component {
        ends_with: "/share/X11/xkb",
        places: &[Root("share/X11/xkb")],
        var: "XKB_CONFIG_ROOT",
    },
    Component {
        ends_with: "/share/file/misc/magic",
        places: MAGIC,
        var: "MAGIC",
    },
    Component {
        ends_with: "/share/misc/magic",
        places: MAGIC,
        var: "MAGIC",
    },
];

/// A compiled-in path no copy in the bundle answers for.
#[derive(Debug, PartialEq)]
pub(crate) struct Finding {
    /// The file holding the path, relative to the tree.
    pub file: PathBuf,
    pub path: String,
    /// Where the bundle could carry the directory instead.
    pub carry: String,
    pub var: &'static str,
}

/// Every known compiled-in path in `elf_files` that the tree under `root`
/// carries no copy of.
pub(crate) fn unanswered(root: &Path, elf_files: &[PathBuf]) -> Vec<Finding> {
    let lib_dirs = library_dirs(root, elf_files);
    let carried = |place: &Place| match place {
        Lib(rel) => lib_dirs.iter().any(|dir| dir.join(rel).exists()),
        Root(rel) => root.join(rel).exists(),
    };
    let mut findings = Vec::new();
    for file in elf_files {
        let Ok(data) = fs::read(file) else {
            continue;
        };
        let rel = file.strip_prefix(root).unwrap_or(file).to_path_buf();
        let mut seen: Vec<&[u8]> = Vec::new();
        for piece in data.split(|&b| b == 0) {
            let Some(component) = component_of(piece) else {
                continue;
            };
            if seen.contains(&piece) || component.places.iter().any(carried) {
                continue;
            }
            seen.push(piece);
            findings.push(Finding {
                file: rel.clone(),
                path: String::from_utf8_lossy(piece).into_owned(),
                carry: describe(component.places),
                var: component.var,
            });
        }
    }
    // GLib builds its module directory at run time, so no string names
    // it. A bundled GIO without modules of its own loads the host's, the
    // dconf settings backend and gvfs among them.
    let gio = elf_files.iter().find(|f| {
        f.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("libgio-2.0.so"))
    });
    if let Some(gio) = gio
        && !lib_dirs.iter().any(|dir| dir.join("gio/modules").is_dir())
    {
        findings.push(Finding {
            file: gio.strip_prefix(root).unwrap_or(gio).to_path_buf(),
            path: "the host's GIO modules (dconf, gvfs)".to_string(),
            carry: describe(&[Lib("gio/modules")]),
            var: "GIO_MODULE_DIR",
        });
    }
    findings
}

/// The component an absolute path names, when it is one the lint knows.
fn component_of(piece: &[u8]) -> Option<&'static Component> {
    if piece.first() != Some(&b'/')
        || piece.len() > 4096
        || !piece.iter().all(|b| b.is_ascii_graphic())
    {
        return None;
    }
    COMPONENTS
        .iter()
        .find(|c| piece.ends_with(c.ends_with.as_bytes()))
}

/// The directories of the tree holding a shared object, other than
/// `bin/`, `share/` and `etc/`: the ones `pack` records as library
/// directories.
fn library_dirs(root: &Path, elf_files: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for file in elf_files {
        let is_object = file
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(".so"));
        let Some(dir) = file.parent().filter(|_| is_object) else {
            continue;
        };
        let top = dir
            .strip_prefix(root)
            .ok()
            .and_then(|r| r.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        let excluded = top
            .as_deref()
            .is_none_or(|t| ["bin", "share", "etc"].contains(&t));
        if !excluded && !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs
}

fn describe(places: &[Place]) -> String {
    places
        .iter()
        .map(|place| match place {
            Lib(rel) => format!("{rel} in a library directory"),
            Root(rel) => (*rel).to_string(),
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Print `findings` as a warning.
pub(crate) fn report(findings: &[Finding]) {
    if findings.is_empty() {
        return;
    }
    eprintln!(
        "{} {} compiled-in plugin or data path(s) have no copy in the bundle:",
        color::bold_red("warning:"),
        findings.len()
    );
    for f in findings {
        eprintln!(
            "  - {} reads {} (carry {} and the runtime sets {})",
            f.file.display(),
            f.path,
            f.carry,
            f.var
        );
    }
    eprintln!(
        "  Without a copy these come from the host, built against the \
         host's libraries, or are missing there."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compiled_in_dir_is_reported_until_the_bundle_carries_it() {
        let root = std::env::temp_dir().join(format!("onelf-datadirs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        let gst = lib.join("libgstreamer-1.0.so.0");
        fs::write(
            &gst,
            b"\x7fELF\0/usr/lib/x86_64-linux-gnu/gstreamer-1.0\0/usr/share/locale\0\
              /usr/share/alsa\0not /usr/lib/gstreamer-1.0\0",
        )
        .unwrap();
        fs::create_dir_all(root.join("share/alsa")).unwrap();
        let files = vec![gst.clone()];

        assert_eq!(
            unanswered(&root, &files),
            vec![Finding {
                file: PathBuf::from("lib/libgstreamer-1.0.so.0"),
                path: "/usr/lib/x86_64-linux-gnu/gstreamer-1.0".into(),
                carry: "gstreamer-1.0 in a library directory".into(),
                var: "GST_PLUGIN_SYSTEM_PATH_1_0",
            }]
        );

        fs::create_dir_all(lib.join("gstreamer-1.0")).unwrap();
        assert!(unanswered(&root, &files).is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bundled_gio_without_modules_is_reported() {
        let root = std::env::temp_dir().join(format!("onelf-gio-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("lib")).unwrap();
        let gio = root.join("lib/libgio-2.0.so.0");
        fs::write(&gio, b"\x7fELF").unwrap();
        let files = vec![gio.clone()];
        let found = unanswered(&root, &files);
        assert!(found.iter().any(|f| f.var == "GIO_MODULE_DIR"), "{found:?}");
        fs::create_dir_all(root.join("lib/gio/modules")).unwrap();
        let found = unanswered(&root, &files);
        let _ = fs::remove_dir_all(&root);
        assert!(
            !found.iter().any(|f| f.var == "GIO_MODULE_DIR"),
            "{found:?}"
        );
    }
}
