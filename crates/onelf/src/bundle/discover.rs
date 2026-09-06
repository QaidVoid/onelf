//! Libraries an application loads at runtime, found by running it.
//!
//! A `dlopen` by computed name leaves nothing in `DT_NEEDED`: SDL picks
//! its audio backend at runtime, glibc opens `libgcc_s` on the first
//! `pthread_cancel`, GTK loads its pixbuf loaders by directory listing.
//! The string scan catches the well-known names; this catches the rest by
//! running the target for a few seconds and asking the loader what it
//! actually loaded.
//!
//! On glibc the loader reports that itself through `LD_DEBUG`, written to
//! a file per process, so every helper the target spawns is covered and
//! nothing else has to be installed. musl's loader has no such switch, so
//! a musl target is traced with `strace`, which has to be present.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::elf::LibcFamily;

/// Run `target` with `args` for at most `seconds`, and return every
/// shared object outside `tree` it loaded, sorted and deduplicated.
///
/// `host_dirs` go on `LD_LIBRARY_PATH` for the run. A tree that was
/// bundled before carries a loader with this machine's directories
/// scrubbed out, so without them a `dlopen` of a host library fails and
/// the run reports nothing; on a fresh tree they change nothing.
pub(crate) fn loaded_libraries(
    target: &Path,
    args: &[String],
    seconds: u64,
    family: Option<LibcFamily>,
    tree: &Path,
    host_dirs: &[PathBuf],
) -> io::Result<Vec<PathBuf>> {
    let scratch = std::env::temp_dir().join(format!("onelf-trace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;
    let library_path = host_dirs
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    let result = match family {
        Some(LibcFamily::Musl) => trace_with_strace(target, args, seconds, &scratch, &library_path),
        _ => trace_with_ld_debug(target, args, seconds, &scratch, &library_path),
    };
    let _ = std::fs::remove_dir_all(&scratch);
    let mut libs: Vec<PathBuf> = result?
        .into_iter()
        .filter(|p| p.is_absolute() && !p.starts_with(tree) && p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".so"))
        })
        .collect();
    libs.sort();
    libs.dedup();
    Ok(libs)
}

/// glibc's loader names every object it maps when asked, one log per
/// process. `files` reports the map as it is made, `libs` the search that
/// led there; either line carries the path that was actually opened.
fn trace_with_ld_debug(
    target: &Path,
    args: &[String],
    seconds: u64,
    scratch: &Path,
    library_path: &str,
) -> io::Result<Vec<PathBuf>> {
    let log = scratch.join("ld");
    let mut cmd = Command::new(target);
    cmd.args(args)
        .env("LD_DEBUG", "libs,files")
        .env("LD_DEBUG_OUTPUT", &log)
        .env("LD_LIBRARY_PATH", library_path);
    run_for(&mut cmd, seconds)?;

    let mut found = Vec::new();
    for entry in std::fs::read_dir(scratch)?.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in text.lines() {
            // Lines read `    12345:\tcalling init: /path`.
            let line = line
                .trim_start()
                .trim_start_matches(|c: char| c.is_ascii_digit() || c == ':')
                .trim_start();
            let path = line.strip_prefix("calling init: ").or_else(|| {
                line.strip_prefix("file=")
                    .and_then(|rest| rest.split_once(" [").map(|(p, _)| p))
                    .filter(|_| line.contains("generating link map"))
            });
            if let Some(path) = path {
                found.push(PathBuf::from(path.trim()));
            }
        }
    }
    Ok(found)
}

/// musl's loader reports nothing, so the opens are watched from outside.
fn trace_with_strace(
    target: &Path,
    args: &[String],
    seconds: u64,
    scratch: &Path,
    library_path: &str,
) -> io::Result<Vec<PathBuf>> {
    let log = scratch.join("strace");
    let mut cmd = Command::new("strace");
    cmd.args(["-f", "-qq", "-e", "trace=open,openat", "-o"])
        .arg(&log)
        .arg(target)
        .args(args)
        .env("LD_LIBRARY_PATH", library_path);
    match run_for(&mut cmd, seconds) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "tracing a musl target needs strace, which is not on PATH",
            ));
        }
        Err(e) => return Err(e),
    }
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    Ok(text
        .lines()
        .filter(|l| {
            l.trim_end()
                .rsplit_once(" = ")
                .is_some_and(|(_, rc)| rc.trim().parse::<i64>().is_ok_and(|n| n >= 0))
        })
        .filter_map(|l| {
            let start = l.find('"')? + 1;
            let end = l[start..].find('"')? + start;
            Some(PathBuf::from(&l[start..end]))
        })
        .collect())
}

/// Run `cmd` detached from this terminal, stop it after `seconds` if it is
/// still going, and wait for it. A target that exits on its own before
/// then is fine; one that has to be stopped gets a chance to shut down
/// cleanly first.
fn run_for(cmd: &mut Command, seconds: u64) -> io::Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
            let grace = Instant::now() + Duration::from_secs(2);
            while child.try_wait()?.is_none() && Instant::now() < grace {
                std::thread::sleep(Duration::from_millis(50));
            }
            if child.try_wait()?.is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}
