//! Shared terminal UI helpers for the bundler: colorized status output and
//! human-readable byte sizes.

pub(crate) mod color {
    use std::io::IsTerminal;
    use std::sync::OnceLock;

    static ENABLED: OnceLock<bool> = OnceLock::new();

    fn enabled() -> bool {
        *ENABLED.get_or_init(|| {
            std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
        })
    }

    pub fn bold(s: &str) -> String {
        if enabled() {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn red(s: &str) -> String {
        if enabled() {
            format!("\x1b[31m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn cyan(s: &str) -> String {
        if enabled() {
            format!("\x1b[36m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn dim(s: &str) -> String {
        if enabled() {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn bold_green(s: &str) -> String {
        if enabled() {
            format!("\x1b[1;32m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn bold_red(s: &str) -> String {
        if enabled() {
            format!("\x1b[1;31m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

/// One attributed finding as a report line: the shared two-space indent,
/// a dash, then the caller's text. The caller passes the text already
/// sorted and attributed, so the line's bytes are a function of the
/// findings alone and a snapshot can pin them.
pub(crate) fn finding_line(text: &str) -> String {
    format!("  - {text}")
}

/// Render a findings report as one stable block: a `warning:` header
/// counting the findings, one line per finding in the order given, and
/// an optional closing note. The caller sorts and attributes the lines,
/// so the block never depends on map or filesystem iteration order.
pub(crate) fn warning_report(
    count: usize,
    subject: &str,
    lines: &[String],
    note: Option<&str>,
) -> String {
    let mut out = format!("{} {count} {subject}:", color::bold_red("warning:"));
    for line in lines {
        out.push('\n');
        out.push_str(line);
    }
    if let Some(note) = note {
        out.push('\n');
        out.push_str("  ");
        out.push_str(note);
    }
    out
}

/// Format a byte count as a human-readable size (B / KB / MB / GB).
pub(crate) fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finding line is its text with the shared indent, byte for byte.
    #[test]
    fn a_finding_line_is_the_text_with_the_shared_indent() {
        assert_eq!(
            finding_line("lib/libx.so reads /usr/share/x"),
            "  - lib/libx.so reads /usr/share/x"
        );
    }

    /// The report block is the count header, one line per finding, and
    /// the note, each on its own line in the order given.
    #[test]
    fn the_report_block_is_the_header_the_lines_and_the_note() {
        let block = warning_report(
            2,
            "subjects with no copy",
            &[finding_line("one"), finding_line("two")],
            Some("the closing note"),
        );
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines.len(), 4, "{block:?}");
        assert!(
            lines[0].ends_with(" 2 subjects with no copy:"),
            "{}",
            lines[0]
        );
        assert_eq!(lines[1], "  - one");
        assert_eq!(lines[2], "  - two");
        assert_eq!(lines[3], "  the closing note");
        let bare = warning_report(1, "subject", &[finding_line("one")], None);
        assert_eq!(bare.lines().count(), 2, "{bare:?}");
    }
}
