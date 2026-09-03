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
}
