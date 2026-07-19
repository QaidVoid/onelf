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
