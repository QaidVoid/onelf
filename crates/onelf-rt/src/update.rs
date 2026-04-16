//! Self-update via zsync.
//!
//! Reads the update URL from `.onelf/update-url` metadata (written at pack
//! time) and uses zsync-rs to perform a delta download seeded by the current
//! executable. The new binary is written to a sibling temp file and moved
//! atomically into place.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use zsync_rs::ZsyncAssembly;

pub enum UpdateFlag {
    /// Just report whether an update is available.
    Check,
    /// Download and apply the update.
    Apply,
}

/// Inspect argv for update flags. Returns the chosen action if matched.
pub fn parse_flag(args: &[String]) -> Option<UpdateFlag> {
    if args.iter().any(|a| a == "--onelf-update") {
        Some(UpdateFlag::Apply)
    } else if args.iter().any(|a| a == "--onelf-check-update") {
        Some(UpdateFlag::Check)
    } else {
        None
    }
}

/// Run the chosen update action against `self_path`, using `update_url`
/// from the package metadata. Returns a process exit code.
pub fn run(flag: UpdateFlag, self_path: &Path, update_url: &str) -> i32 {
    match flag {
        UpdateFlag::Check => check(self_path, update_url),
        UpdateFlag::Apply => apply(self_path, update_url),
    }
}

fn check(self_path: &Path, url: &str) -> i32 {
    match fetch_control(url) {
        Ok(ctl) => {
            let current_sha = sha1_file_hex(self_path).unwrap_or_default();
            let remote_sha = ctl.sha1.clone().unwrap_or_default();
            if !remote_sha.is_empty() && current_sha == remote_sha {
                println!("up to date ({current_sha})");
                0
            } else {
                println!("update available: {current_sha} -> {remote_sha}");
                1
            }
        }
        Err(e) => {
            eprintln!("onelf-rt: check update: {e}");
            2
        }
    }
}

fn apply(self_path: &Path, url: &str) -> i32 {
    let ctl = match fetch_control(url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("onelf-rt: fetch control: {e}");
            return 2;
        }
    };

    let current_sha = sha1_file_hex(self_path).unwrap_or_default();
    let remote_sha = ctl.sha1.clone().unwrap_or_default();
    if !remote_sha.is_empty() && current_sha == remote_sha {
        println!("already up to date ({current_sha})");
        return 0;
    }

    let tmp_path = self_path.with_extension("onelf-update.tmp");
    eprintln!("onelf-rt: downloading update to {}", tmp_path.display());

    let mut assembly = match ZsyncAssembly::from_url(url, &tmp_path) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("onelf-rt: init assembly: {e}");
            return 2;
        }
    };

    // Seed from our own file: most blocks usually match.
    if let Err(e) = assembly.submit_source_file(self_path) {
        eprintln!("onelf-rt: warning: seeding failed: {e}");
    }

    while !assembly.is_complete() {
        match assembly.download_missing_blocks() {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("onelf-rt: download error: {e}");
                let _ = std::fs::remove_file(&tmp_path);
                return 2;
            }
        }
    }

    if let Err(e) = assembly.complete() {
        eprintln!("onelf-rt: verify failed: {e}");
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    }

    // Preserve executable bit.
    let mode = std::fs::metadata(self_path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o755);
    if let Err(e) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode)) {
        eprintln!("onelf-rt: chmod failed: {e}");
    }

    // Atomic replace.
    if let Err(e) = std::fs::rename(&tmp_path, self_path) {
        eprintln!("onelf-rt: rename failed: {e}");
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    }

    println!("updated: {current_sha} -> {remote_sha}");
    0
}

fn fetch_control(url: &str) -> Result<zsync_rs::ControlFile, String> {
    let client = zsync_rs::HttpClient::new();
    client.fetch_control_file(url).map_err(|e| format!("{e}"))
}

fn sha1_file_hex(path: &Path) -> io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let digest = zsync_rs::checksum::calc_sha1_stream(&mut f)?;
    let mut s = String::with_capacity(40);
    for b in digest.iter() {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

/// Resolve this binary's path on disk via `/proc/self/exe`.
pub fn self_path() -> Option<PathBuf> {
    std::fs::read_link("/proc/self/exe").ok()
}
