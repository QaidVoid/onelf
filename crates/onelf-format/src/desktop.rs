//! Desktop Entry `Exec=` quoting and the icon set, shared by the packer's
//! and the runtime's integrators so a binary under a path with a space
//! gets one menu entry that launches, from either side.

use crate::{EntryKind, Manifest, is_safe_component};

/// Quote one `Exec=` field argument per the Desktop Entry spec, ready to
/// write into the file: a value containing whitespace or a reserved
/// character is double-quoted, with `"`, `` ` ``, `$`, and `\`
/// backslash-escaped inside the quotes, and `%` becomes `%%` so it is not
/// read as a field code. The result then goes through [`string_value`],
/// because `Exec` is a string key whose escapes are undone before the
/// quoting is: a literal backslash in a quoted argument takes four.
pub fn exec_arg(s: &str) -> String {
    let reserved = |c: char| c.is_whitespace() || "\"'\\<>~|&;$*?#()`".contains(c);
    let quote = s.is_empty() || s.chars().any(reserved);
    let mut out = String::with_capacity(s.len() + 2);
    if quote {
        out.push('"');
    }
    for c in s.chars() {
        if quote && matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        if c == '%' {
            out.push('%');
        }
        out.push(c);
    }
    if quote {
        out.push('"');
    }
    string_value(&out)
}

/// Escape `s` as a Desktop Entry string value: a backslash, newline, tab
/// or carriage return becomes its backslash escape, and a leading space
/// becomes `\s` so it is not trimmed. This is the whole encoding for a
/// plain string key such as `TryExec`.
pub fn string_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            ' ' if i == 0 => out.push_str("\\s"),
            c => out.push(c),
        }
    }
    out
}

/// Given the value of an `Exec=` line (everything after `Exec=`), the
/// argument tail after the first argument (the executable), honouring
/// double-quote quoting so a quoted or space-containing executable path
/// is removed as one whole argument rather than split on its spaces.
pub fn exec_arg_tail(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'"' {
        // Quoted argument: skip to the matching unescaped closing quote.
        // The value is still string-escaped, so `\\` is one backslash,
        // which in turn escapes whatever follows it.
        i += 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if i + 1 < bytes.len() => {
                    let backslash = bytes[i + 1] == b'\\';
                    i += 2;
                    if backslash && i < bytes.len() {
                        i += if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            2
                        } else {
                            1
                        };
                    }
                }
                b'"' => {
                    i += 1;
                    break;
                }
                _ => i += 1,
            }
        }
    } else {
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    &value[i..]
}

/// The `Exec=` command that launches `path`, through `entrypoint` when
/// given. A package picks its entrypoint by the name it is run as, so an
/// entrypoint the path would not reach is named in `ONELF_ENTRYPOINT`.
pub fn exec_command(path: &str, entrypoint: Option<&str>) -> String {
    match entrypoint {
        Some(name) => format!(
            "env {} {}",
            exec_arg(&format!("{ENTRYPOINT_VAR}={name}")),
            exec_arg(path)
        ),
        None => exec_arg(path),
    }
}

/// The variable a desktop entry sets to launch a chosen entrypoint.
const ENTRYPOINT_VAR: &str = "ONELF_ENTRYPOINT";

/// `value` without the `env ONELF_ENTRYPOINT=...` an earlier patch wrote
/// ahead of the binary, so re-patching replaces the binary and not `env`.
fn without_entrypoint_prefix(value: &str) -> &str {
    let Some(rest) = value.strip_prefix("env ") else {
        return value;
    };
    let rest = rest.trim_start();
    let token = rest.split_whitespace().next().unwrap_or_default();
    if token.trim_start_matches('"').starts_with(ENTRYPOINT_VAR) {
        rest[token.len()..].trim_start()
    } else {
        value
    }
}

/// Patch an existing desktop file for this package's launch.
///
/// `Exec=` is rewritten to launch the package binary (quoting it as the
/// spec requires), keeping any field codes or arguments that followed.
/// `TryExec=` gets the path with only string escaping: the spec defines
/// it as a literal path, not an Exec field, so field-code quoting does
/// not apply.
/// `Icon=` becomes `icon_name` when given. `entrypoint` is passed to
/// [`exec_command`].
///
/// Each `[Desktop Action ...]` group's `Exec=` is rewritten the same way,
/// keeping the action's own arguments, so a menu action launches the
/// package as the entry does. Nothing else in an action is touched, and
/// neither an action's command nor its presence decides what the entry
/// group gets. Missing keys are inserted right after the
/// `[Desktop Entry]` header, so they land in that group even when it is
/// not the last one.
pub fn patch_desktop_file(
    content: &str,
    exec_path: &str,
    entrypoint: Option<&str>,
    icon_name: Option<&str>,
) -> String {
    let quoted = exec_command(exec_path, entrypoint);
    let try_exec = string_value(exec_path);
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let mut has_exec = false;
    let mut has_tryexec = false;
    let mut in_entry = false;
    let mut in_action = false;

    for line in &mut lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_entry = trimmed == "[Desktop Entry]";
            in_action = trimmed.starts_with("[Desktop Action ");
            continue;
        }
        if !in_entry && !in_action {
            continue;
        }
        if line.starts_with("Exec=") {
            // Replace the executable (the first argument), keeping the
            // rest of the command line verbatim.
            let tail = exec_arg_tail(without_entrypoint_prefix(&line[5..]));
            if tail.is_empty() {
                *line = format!("Exec={quoted}");
            } else {
                *line = format!("Exec={quoted} {tail}");
            }
            has_exec |= in_entry;
        } else if in_action {
            continue;
        } else if line.starts_with("TryExec=") {
            *line = format!("TryExec={try_exec}");
            has_tryexec = true;
        } else if line.starts_with("Icon=")
            && let Some(name) = icon_name
        {
            *line = format!("Icon={name}");
        }
    }

    let mut insert_at = lines
        .iter()
        .position(|l| l.trim() == "[Desktop Entry]")
        .map(|h| h + 1)
        .unwrap_or(lines.len());
    if !has_exec {
        lines.insert(insert_at, format!("Exec={quoted}"));
        insert_at += 1;
    }
    if !has_tryexec {
        lines.insert(insert_at, format!("TryExec={try_exec}"));
    }

    let mut result = lines.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// The `Icon=` of the `[Desktop Entry]` group, when it names a theme icon
/// rather than a file.
pub fn entry_icon(content: &str) -> Option<&str> {
    let mut in_entry = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_entry = trimmed == "[Desktop Entry]";
        } else if in_entry && let Some(value) = line.strip_prefix("Icon=") {
            let value = value.trim();
            return (!value.is_empty() && !value.contains('/')).then_some(value);
        }
    }
    None
}

/// One icon to install into the user's hicolor theme.
pub struct ThemeIcon {
    /// The theme's size directory, such as `48x48` or `scalable`.
    pub size: String,
    /// `png` or `svg`.
    pub ext: &'static str,
    pub data: Vec<u8>,
}

/// The entrypoints other than the default that carry a desktop file of
/// their own, `.onelf/desktop/<name>.desktop`, in manifest order.
/// Integrating a package installs these beside the default's entry. One
/// served only by the `default.desktop` fallback is left out, so a
/// command line entrypoint does not become a second copy of the app's
/// menu entry.
pub fn menu_entrypoints(manifest: &Manifest) -> Vec<&str> {
    let desktops: Vec<String> = (0..manifest.entries.len())
        .filter(|&i| manifest.entries[i].kind == EntryKind::File)
        .map(|i| manifest.entry_path(i))
        .filter(|path| path.starts_with(".onelf/desktop/"))
        .collect();
    let default = manifest.header.default_entrypoint as usize;
    manifest
        .entrypoints
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != default)
        .map(|(_, ep)| manifest.get_string(ep.name))
        .filter(|name| {
            let own = format!(".onelf/desktop/{name}.desktop");
            desktops.contains(&own)
        })
        .collect()
}

/// Every icon a package carries for one desktop entry.
///
/// That is each `icons/hicolor/<size>/apps/<name>.png` or `.svg` the
/// package ships, for the first of `names` that has any, and the
/// `.onelf/icons` icon `bundled` at the size its file gives. The latter is
/// the one published for integration, so it wins over a theme icon of the
/// same size. `read` returns an entry's verified bytes. A file that is
/// neither SVG nor PNG is left out, since the theme would show a broken
/// image.
pub fn icon_set(
    manifest: &Manifest,
    names: &[&str],
    bundled: Option<usize>,
    mut read: impl FnMut(usize) -> std::io::Result<Vec<u8>>,
) -> std::io::Result<Vec<ThemeIcon>> {
    let paths: Vec<(usize, String)> = (0..manifest.entries.len())
        .filter(|&i| manifest.entries[i].kind == EntryKind::File)
        .map(|i| (i, manifest.entry_path(i)))
        .collect();
    let mut found: Vec<(String, &'static str, usize)> = Vec::new();
    for name in names {
        for (i, path) in &paths {
            if let Some((size, ext)) = theme_slot(path, name)
                && !found.iter().any(|(s, ..)| *s == size)
            {
                found.push((size.to_string(), ext, *i));
            }
        }
        if !found.is_empty() {
            break;
        }
    }
    let mut icons = Vec::new();
    for (size, ext, i) in found {
        let data = read(i)?;
        if ext == "svg" || png_dimensions(&data).is_some() {
            icons.push(ThemeIcon { size, ext, data });
        }
    }
    if let Some(i) = bundled {
        let data = read(i)?;
        let path = manifest.entry_path(i);
        let slot = if path.ends_with(".svg") {
            Some(("scalable".to_string(), "svg"))
        } else {
            png_dimensions(&data).map(|(w, h)| (format!("{w}x{h}"), "png"))
        };
        match slot {
            Some((size, ext)) => {
                icons.retain(|icon| icon.size != size);
                icons.push(ThemeIcon { size, ext, data });
            }
            None => eprintln!("  Icon {path}: neither SVG nor a decodable PNG; not installed"),
        }
    }
    Ok(icons)
}

/// The size directory and extension of `path` when it is
/// `icons/hicolor/<size>/apps/<name>.png` or `.svg`, anywhere in the tree.
fn theme_slot<'p>(path: &'p str, name: &str) -> Option<(&'p str, &'static str)> {
    const THEME: &str = "icons/hicolor/";
    let at = if path.starts_with(THEME) {
        0
    } else {
        path.find("/icons/hicolor/")? + 1
    };
    let (size, file) = path[at + THEME.len()..].split_once("/apps/")?;
    let ext = match file.strip_prefix(name)? {
        ".png" => "png",
        ".svg" => "svg",
        _ => return None,
    };
    is_safe_component(size).then_some((size, ext))
}

/// Width and height from a PNG's IHDR chunk.
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || data[0..8] != *b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Entry, EntryPoint, EntryPointFlags, ManifestHeader, StringTableBuilder, WorkingDir,
    };

    /// A manifest with entrypoints `eps` (the first the default) and the
    /// files `desktops` under `.onelf/desktop/`.
    fn with_desktops(eps: &[&str], desktops: &[&str]) -> Manifest {
        let mut strings = StringTableBuilder::new();
        let dir = |name: u32, parent: u32| Entry {
            kind: EntryKind::Dir,
            parent,
            name,
            mode: 0o755,
            mtime_secs: 0,
            mtime_nsec: 0,
            content_hash: [0; 32],
            blocks: Vec::new(),
            symlink_target: 0,
        };
        let mut entries = vec![dir(strings.add(".onelf"), u32::MAX)];
        entries.push(dir(strings.add("desktop"), 0));
        for file in desktops {
            entries.push(Entry {
                kind: EntryKind::File,
                mode: 0o644,
                ..dir(strings.add(file), 1)
            });
        }
        let entrypoints = eps
            .iter()
            .map(|name| EntryPoint {
                name: strings.add(name),
                target_entry: 0,
                args: 0,
                working_dir: WorkingDir::Inherit,
                flags: EntryPointFlags::empty(),
            })
            .collect::<Vec<_>>();
        let string_table = strings.finish();
        Manifest {
            header: ManifestHeader {
                version: crate::manifest::MANIFEST_VERSION,
                entry_count: entries.len() as u32,
                string_table_size: string_table.len() as u32,
                entrypoint_count: entrypoints.len() as u16,
                default_entrypoint: 0,
                lib_dir_count: 0,
                name_offset: 0,
                package_id: [0; 32],
                prefix_hash: [0; 32],
                footer_hash: [0; 32],
            },
            entrypoints,
            entries,
            lib_dir_offsets: Vec::new(),
            string_table,
        }
    }

    #[test]
    fn an_entrypoint_with_its_own_desktop_file_has_a_menu_entry() {
        let m = with_desktops(
            &["app", "settings", "cli"],
            &["app.desktop", "settings.desktop"],
        );
        assert_eq!(menu_entrypoints(&m), vec!["settings"]);
    }

    #[test]
    fn the_default_desktop_file_gives_no_extra_menu_entry() {
        let m = with_desktops(&["app", "cli"], &["default.desktop"]);
        assert!(menu_entrypoints(&m).is_empty());
    }

    #[test]
    fn a_package_without_desktop_files_has_no_extra_menu_entry() {
        let m = with_desktops(&["app", "cli"], &[]);
        assert!(menu_entrypoints(&m).is_empty());
    }

    #[test]
    fn exec_arg_tail_handles_quoting() {
        assert_eq!(exec_arg_tail("/usr/bin/foo %U"), "%U");
        assert_eq!(exec_arg_tail("\"/opt/my app/bin\" %F extra"), "%F extra");
        assert_eq!(exec_arg_tail("/usr/bin/foo"), "");
        assert_eq!(exec_arg_tail("\"/opt/my app/bin\""), "");
        assert_eq!(exec_arg_tail("  /usr/bin/foo  %U"), "%U");
    }

    #[test]
    fn exec_arg_quotes_only_what_needs_it() {
        assert_eq!(exec_arg("/opt/app/bin"), "/opt/app/bin");
        assert_eq!(exec_arg("/opt/my app/bin"), "\"/opt/my app/bin\"");
        assert_eq!(exec_arg(""), "\"\"");
    }

    /// The exec escapes sit inside a string value, so each of their
    /// backslashes is doubled, and `%` is doubled so it is not a field code.
    #[test]
    fn exec_arg_escapes_for_both_levels_and_field_codes() {
        assert_eq!(exec_arg(r#"/a"b$c"#), r#""/a\\"b\\$c""#);
        assert_eq!(exec_arg(r"/a\b"), r#""/a\\\\b""#);
        assert_eq!(exec_arg("/opt/100%/app"), "/opt/100%%/app");
        assert_eq!(exec_arg("/opt/50% off/app"), r#""/opt/50%% off/app""#);
        assert_eq!(exec_arg("/a\nb"), r#""/a\nb""#);
        assert_eq!(string_value(r"/a\b"), r"/a\\b");
        assert_eq!(string_value(" a"), r"\sa");
    }

    /// What `exec_arg` writes, `exec_arg_tail` must read back as one
    /// argument, or patching an already patched file splits it.
    #[test]
    fn exec_arg_tail_skips_what_exec_arg_writes() {
        for path in [r#"/a"b c"#, r"/a\ b", r"/a b\", r#"/x y\"z"#] {
            let line = format!("{} %U", exec_arg(path));
            assert_eq!(exec_arg_tail(&line), "%U", "{line}");
        }
    }

    #[test]
    fn tryexec_is_string_escaped() {
        let out = patch_desktop_file("[Desktop Entry]\n", r"/opt/a\b/app", None, None);
        assert!(out.contains(r"TryExec=/opt/a\\b/app"), "{out}");
        assert!(out.contains(r#"Exec="/opt/a\\\\b/app""#), "{out}");
    }

    #[test]
    fn patch_replaces_exec_keeps_tail_and_icon() {
        let desktop = "[Desktop Entry]\nType=Application\nExec=\"/opt/my app/bin\" %F\nIcon=old\n";
        let out = patch_desktop_file(desktop, "/new/path/app", None, Some("newicon"));
        assert!(out.contains("Exec=/new/path/app %F"), "got:\n{out}");
        assert!(!out.contains("app/bin"), "stale path fragment left:\n{out}");
        assert!(out.contains("Icon=newicon"));
    }

    #[test]
    fn a_spaced_path_is_quoted_in_exec_and_literal_in_tryexec() {
        let desktop = "[Desktop Entry]\nExec=/usr/bin/old\nTryExec=/usr/bin/old\n";
        let out = patch_desktop_file(desktop, "/opt/has space/app", None, None);
        assert!(out.contains("Exec=\"/opt/has space/app\"\n"), "got:\n{out}");
        assert!(
            out.contains("TryExec=/opt/has space/app\n"),
            "TryExec is a literal path, not a quoted field: {out}"
        );
    }

    #[test]
    fn an_exec_in_an_action_group_does_not_speak_for_the_entry() {
        // The entry carries no Exec; the action does. The missing keys
        // must land in the entry group, not at EOF inside the action.
        let desktop = "[Desktop Entry]\nType=Application\nName=App\n\
                      [Desktop Action new-window]\nExec=/opt/old/app --new\n";
        let out = patch_desktop_file(desktop, "/opt/new/app", None, None);
        let entry_end = out.find("[Desktop Action").unwrap();
        let entry = &out[..entry_end];
        let action = &out[entry_end..];
        assert!(
            entry.contains("Exec=/opt/new/app\n"),
            "the entry got its Exec: {out}"
        );
        assert!(
            entry.contains("TryExec=/opt/new/app\n"),
            "the entry got its TryExec: {out}"
        );
        assert!(
            action.contains("Exec=/opt/new/app --new\n"),
            "the action launches the package with its own arguments: {out}"
        );
        assert!(!action.contains("TryExec"), "{out}");
    }

    /// An action's Exec still named the command it was written with, so
    /// every menu action of an integrated package failed to start.
    #[test]
    fn actions_launch_the_package_through_the_entrypoint() {
        let desktop = "[Desktop Entry]\nExec=myapp %U\nActions=new;private;\n\
                       [Desktop Action new]\nName=New\nExec=myapp --new-window %U\nIcon=keep\n\
                       [Desktop Action private]\nExec=\"/opt/my app/app\" --private\n\
                       [X-Other]\nExec=leave me\n";
        let out = patch_desktop_file(desktop, "/opt/app.onelf", Some("daemon"), Some("icon"));
        let launch = "env ONELF_ENTRYPOINT=daemon /opt/app.onelf";
        assert!(out.contains(&format!("Exec={launch} %U\n")), "{out}");
        assert!(
            out.contains(&format!("Exec={launch} --new-window %U\n")),
            "{out}"
        );
        assert!(out.contains(&format!("Exec={launch} --private\n")), "{out}");
        assert!(
            out.contains("Icon=keep\n"),
            "an action's icon is its own: {out}"
        );
        assert!(
            out.contains("Exec=leave me\n"),
            "other groups are left: {out}"
        );
        assert_eq!(out.matches("TryExec=").count(), 1, "{out}");

        let again = patch_desktop_file(&out, "/new/app.onelf", None, None);
        assert!(
            again.contains("Exec=/new/app.onelf --new-window %U\n"),
            "{again}"
        );
        assert!(again.contains("Exec=/new/app.onelf --private\n"), "{again}");
    }

    #[test]
    fn theme_icons_are_found_at_every_size() {
        assert_eq!(
            theme_slot("share/icons/hicolor/48x48/apps/app.png", "app"),
            Some(("48x48", "png"))
        );
        assert_eq!(
            theme_slot("icons/hicolor/scalable/apps/app.svg", "app"),
            Some(("scalable", "svg"))
        );
        assert_eq!(
            theme_slot("usr/share/icons/hicolor/32x32/apps/app.xpm", "app"),
            None
        );
        assert_eq!(
            theme_slot("share/icons/hicolor/48x48/apps/app2.png", "app"),
            None
        );
        assert_eq!(
            theme_slot("share/icons/hicolor/../apps/app.png", "app"),
            None
        );
        assert_eq!(
            entry_icon("[Desktop Entry]\nIcon=app\n[Desktop Action a]\nIcon=b\n"),
            Some("app")
        );
        assert_eq!(entry_icon("[Desktop Entry]\nIcon=/opt/app.png\n"), None);
    }

    #[test]
    fn a_chosen_entrypoint_is_named_in_the_launch() {
        let desktop = "[Desktop Entry]\nName=App\nExec=old %U\n";
        let out = patch_desktop_file(desktop, "/opt/app.onelf", Some("daemon"), None);
        assert!(
            out.contains("Exec=env ONELF_ENTRYPOINT=daemon /opt/app.onelf %U"),
            "{out}"
        );
        assert!(out.contains("TryExec=/opt/app.onelf"), "{out}");

        // Patching again, as a moved package does, replaces the binary
        // after the prefix and keeps the field codes.
        let again = patch_desktop_file(&out, "/new/app.onelf", Some("daemon"), None);
        assert!(
            again.contains("Exec=env ONELF_ENTRYPOINT=daemon /new/app.onelf %U"),
            "{again}"
        );
        let plain = patch_desktop_file(&out, "/new/app.onelf", None, None);
        assert!(plain.contains("Exec=/new/app.onelf %U"), "{plain}");
    }
}
