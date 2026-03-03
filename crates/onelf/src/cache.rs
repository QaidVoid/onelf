//! Cache management for ONELF runtime extraction cache.
//!
//! Provides commands to list cached packages, clear the entire cache,
//! and garbage-collect stale entries based on last-used timestamps.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("onelf")
}

pub fn cache_list() -> io::Result<()> {
    let pkg_dir = cache_dir().join("pkg");
    if !pkg_dir.exists() {
        println!("No cached packages.");
        return Ok(());
    }

    let mut count = 0u64;
    let mut total_size = 0u64;

    for entry in fs::read_dir(&pkg_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name();
            let meta_file = cache_dir().join("meta").join(name.to_str().unwrap_or("?"));
            let last_used = if meta_file.exists() {
                fs::metadata(&meta_file)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| {
                        format!(
                            "{}s ago",
                            SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_secs()
                                - d.as_secs()
                        )
                    })
                    .unwrap_or_else(|| "unknown".into())
            } else {
                "unknown".into()
            };

            println!(
                "  {} (last used: {})",
                name.to_str().unwrap_or("?"),
                last_used
            );
            count += 1;
        }
    }

    let cas_dir = cache_dir().join("cas");
    if cas_dir.exists() {
        for shard in fs::read_dir(&cas_dir)? {
            let shard = shard?;
            if shard.file_type()?.is_dir() {
                for file in fs::read_dir(shard.path())? {
                    let file = file?;
                    total_size += file.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }

    println!();
    println!(
        "{} packages, CAS total: {:.1} MB",
        count,
        total_size as f64 / 1_048_576.0
    );
    Ok(())
}

pub fn cache_clear() -> io::Result<()> {
    let dir = cache_dir();
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
        println!("Cache cleared.");
    } else {
        println!("No cache to clear.");
    }
    Ok(())
}

pub fn cache_gc(max_age_days: u64) -> io::Result<()> {
    let now = SystemTime::now();
    let max_age = std::time::Duration::from_secs(max_age_days * 86400);
    let meta_dir = cache_dir().join("meta");
    let pkg_dir = cache_dir().join("pkg");

    if !meta_dir.exists() || !pkg_dir.exists() {
        println!("No cached packages.");
        return Ok(());
    }

    let mut removed = 0u64;

    for entry in fs::read_dir(&meta_dir)? {
        let entry = entry?;
        let modified = entry.metadata()?.modified()?;
        if let Ok(age) = now.duration_since(modified) {
            if age > max_age {
                let name = entry.file_name();
                let pkg = pkg_dir.join(&name);
                if pkg.exists() {
                    fs::remove_dir_all(&pkg)?;
                }
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
    }

    println!(
        "Removed {} stale packages (older than {} days).",
        removed, max_age_days
    );
    Ok(())
}
