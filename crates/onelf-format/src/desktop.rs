//! Desktop Entry `Exec=` quoting, shared by the packer's and the runtime's
//! integrators so a binary under a path with a space gets one menu entry
//! that launches, from either side.

/// Quote one `Exec=` field argument per the Desktop Entry spec: a value
/// containing whitespace or a reserved character is double-quoted, with
/// `"`, `` ` ``, `$`, and `\` backslash-escaped inside the quotes.
pub fn exec_arg(s: &str) -> String {
    let reserved = |c: char| c.is_whitespace() || "\"'\\<>~|&;$*?#()`".contains(c);
    if !s.is_empty() && !s.chars().any(reserved) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
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
        i += 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if i + 1 < bytes.len() => i += 2,
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

/// Patch an existing desktop file for this package's launch.
///
/// `Exec=` is rewritten to launch the package binary (quoting it as the
/// spec requires), keeping any field codes or arguments that followed.
/// `TryExec=` gets the plain path: the spec defines it as a literal
/// path, not an Exec field, so field-code quoting does not apply.
/// `Icon=` becomes `icon_name` when given.
///
/// Only `[Desktop Entry]` is touched. A desktop file may carry further
/// groups (`[Desktop Action ...]`) with `Exec=` lines of their own, and
/// neither their commands nor their presence may decide what the entry
/// group gets. Missing keys are inserted right after the
/// `[Desktop Entry]` header, so they land in that group even when it is
/// not the last one.
pub fn patch_desktop_file(content: &str, exec_path: &str, icon_name: Option<&str>) -> String {
    let quoted = exec_arg(exec_path);
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
            let tail = exec_arg_tail(&line[5..]);
            if tail.is_empty() {
                *line = format!("Exec={quoted}");
            } else {
                *line = format!("Exec={quoted} {tail}");
            }
            has_exec = true;
        } else if line.starts_with("TryExec=") {
            *line = format!("TryExec={exec_path}");
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
        lines.insert(insert_at, format!("TryExec={exec_path}"));
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
        assert_eq!(exec_arg("/a\"b$c"), "\"/a\\\"b\\$c\"");
        assert_eq!(exec_arg(""), "\"\"");
    }

    #[test]
    fn patch_replaces_exec_keeps_tail_and_icon() {
        let desktop = "[Desktop Entry]\nType=Application\nExec=\"/opt/my app/bin\" %F\nIcon=old\n";
        let out = patch_desktop_file(desktop, "/new/path/app", Some("newicon"));
        assert!(out.contains("Exec=/new/path/app %F"), "got:\n{out}");
        assert!(!out.contains("app/bin"), "stale path fragment left:\n{out}");
        assert!(out.contains("Icon=newicon"));
    }

    #[test]
    fn a_spaced_path_is_quoted_in_exec_and_literal_in_tryexec() {
        let desktop = "[Desktop Entry]\nExec=/usr/bin/old\nTryExec=/usr/bin/old\n";
        let out = patch_desktop_file(desktop, "/opt/has space/app", None);
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
        let out = patch_desktop_file(desktop, "/opt/new/app", None);
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
}
