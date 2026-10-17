//! Desktop Entry `Exec=` quoting, shared by the packer's and the runtime's
//! integrators so a binary under a path with a space gets one menu entry
//! that launches, from either side.

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
/// Only `[Desktop Entry]` is touched. A desktop file may carry further
/// groups (`[Desktop Action ...]`) with `Exec=` lines of their own, and
/// neither their commands nor their presence may decide what the entry
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

    for line in &mut lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_entry = trimmed == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
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
            has_exec = true;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // The entry carries no Exec; the action does. The action's Exec
        // must survive untouched, and the missing keys must land in the
        // entry group, not at EOF inside the action.
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
            action.contains("Exec=/opt/old/app --new"),
            "the action's own Exec survives: {out}"
        );
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
