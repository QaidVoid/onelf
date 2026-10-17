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
//!
//! The run is kept off the user's own state: it gets a scratch home, XDG
//! directories and working directory, it runs in a process group of its
//! own, and the whole group is stopped when the time is up. Only what the
//! tree's own programs load counts. A host program the target starts, a
//! shell or `xdg-open`, inherits the tracing too, and its libraries are
//! not the application's.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::color;
use super::elf::LibcFamily;

/// Run `target` with `args` for at most `seconds`, and return every
/// shared object outside `tree` that the tree's own programs loaded,
/// sorted and deduplicated.
///
/// `library_path` goes on `LD_LIBRARY_PATH` for the run, in order. The
/// caller puts the tree's own library directories first, since the
/// variable is searched before any object's run path and a host copy
/// would otherwise stand in for one the tree ships. `loader` starts the
/// target when the interpreter it names is not on this machine.
///
/// A run that ends by itself with a failure is reported, with the end of
/// what it wrote to stderr, since it may have stopped before loading what
/// it would load in use.
pub(crate) fn loaded_libraries(
    target: &Path,
    args: &[String],
    seconds: u64,
    family: Option<LibcFamily>,
    tree: &Path,
    library_path: &[PathBuf],
    loader: Option<&Path>,
) -> io::Result<Vec<PathBuf>> {
    // The loader names absolute paths, and the run's working directory is
    // the scratch home, so both sides of every comparison are absolute.
    let tree = tree.canonicalize()?;
    let target = target.canonicalize()?;
    let scratch = super::private_temp_dir("trace")?;
    let result = trace(
        &target,
        args,
        seconds,
        family,
        &tree,
        library_path,
        loader,
        &scratch,
    );
    let _ = fs::remove_dir_all(&scratch);
    let mut libs: Vec<PathBuf> = result?
        .into_iter()
        .filter(|p| p.is_absolute() && !p.starts_with(&tree) && p.is_file())
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

// One run's whole context; it exists to keep the scratch cleanup in one
// place above.
#[allow(clippy::too_many_arguments)]
fn trace(
    target: &Path,
    args: &[String],
    seconds: u64,
    family: Option<LibcFamily>,
    tree: &Path,
    library_path: &[PathBuf],
    loader: Option<&Path>,
    scratch: &Path,
) -> io::Result<Vec<PathBuf>> {
    let home = scratch.join("home");
    let logs = scratch.join("logs");
    fs::create_dir(&home)?;
    fs::create_dir(&logs)?;
    let strace_log = logs.join("strace");
    let musl = family == Some(LibcFamily::Musl);

    let mut cmd = if musl {
        let mut cmd = Command::new("strace");
        cmd.args(["-f", "-qq", "-e", "trace=execve,open,openat", "-o"])
            .arg(&strace_log);
        cmd.args(loader).arg(target);
        cmd
    } else {
        let mut cmd = Command::new(loader.unwrap_or(target));
        if loader.is_some() {
            cmd.arg(target);
        }
        cmd.env("LD_DEBUG", "libs,files")
            .env("LD_DEBUG_OUTPUT", logs.join("ld"));
        cmd
    };
    let library_path = library_path
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    cmd.args(args)
        .current_dir(&home)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LD_LIBRARY_PATH", library_path);

    let stderr_log = scratch.join("stderr");
    let failed = match run_for(&mut cmd, seconds, File::create(&stderr_log)?) {
        Ok(failed) => failed,
        Err(e) if musl && e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "tracing a musl target needs strace, which is not on PATH",
            ));
        }
        Err(e) => {
            return Err(io::Error::new(
                e.kind(),
                format!("running {}: {e}", target.display()),
            ));
        }
    };
    if let Some(status) = failed {
        let stderr = fs::read_to_string(&stderr_log).unwrap_or_default();
        let lines: Vec<&str> = stderr.lines().collect();
        eprintln!(
            "  {} the run ended by itself with {status}; it may not have loaded \
             everything it does in use",
            color::bold_red("warning:"),
        );
        for line in &lines[lines.len().saturating_sub(5)..] {
            eprintln!("    {line}");
        }
    }

    let ours = |program: &Path| program.starts_with(tree) || Some(program) == loader;
    if musl {
        let text = fs::read_to_string(&strace_log).unwrap_or_default();
        return Ok(parse_strace(&text, ours));
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(&logs)?.flatten() {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let (libs, programs) = parse_ld_debug(&text);
        // A log names its programs by `argv[0]`. One that can be placed
        // outside the tree is a host program the target started, and only
        // a log whose every program is such is left out.
        let foreign = !programs.is_empty()
            && programs
                .iter()
                .all(|p| locate_program(p, &home).is_some_and(|p| !ours(&p)));
        if !foreign {
            found.extend(libs);
        }
    }
    Ok(found)
}

/// glibc's loader names every object it maps when asked, one log per
/// process. `files` reports the map as it is made, `libs` the search that
/// led there; either line carries the path that was actually opened.
/// Returns the paths, and the programs the log ran by the name the loader
/// gives them, which is their `argv[0]`. A process that execs another
/// program goes on writing the same log.
fn parse_ld_debug(text: &str) -> (Vec<PathBuf>, Vec<&str>) {
    let mut found = Vec::new();
    let mut programs = Vec::new();
    for line in text.lines() {
        // Lines read `    12345:\tcalling init: /path`.
        let line = line
            .trim_start()
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == ':')
            .trim_start();
        if let Some(program) = line.strip_prefix("initialize program: ") {
            programs.push(program.trim());
            continue;
        }
        let path = line.strip_prefix("calling init: ").or_else(|| {
            line.strip_prefix("file=")
                .and_then(|rest| rest.split_once(" [").map(|(p, _)| p))
                .filter(|_| line.contains("generating link map"))
        });
        if let Some(path) = path {
            found.push(PathBuf::from(path.trim()));
        }
    }
    (found, programs)
}

/// Where the program the loader names `argv0` lives, when that can be
/// told. A shell fills `argv[0]` from what it found on `PATH`, and the run
/// inherits this process's `PATH`.
fn locate_program(argv0: &str, cwd: &Path) -> Option<PathBuf> {
    let path = Path::new(argv0);
    if path.is_absolute() {
        return path.canonicalize().ok();
    }
    if argv0.contains('/') {
        return cwd.join(path).canonicalize().ok();
    }
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .map(|dir| dir.join(argv0))
        .find(|candidate| candidate.is_file())
        .and_then(|found| found.canonicalize().ok())
}

/// musl's loader reports nothing, so the opens are watched from outside.
///
/// With `-f` a call that another process interrupts is split over two
/// lines, `<unfinished ...>` and `<... resumed>`, which are joined again
/// here by pid. `execve` tells which program each process runs, and the
/// opens of one running a program `ours` does not accept are left out.
fn parse_strace(text: &str, ours: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut pending: HashMap<&str, &str> = HashMap::new();
    let mut foreign: HashSet<&str> = HashSet::new();
    let mut found = Vec::new();
    for line in text.lines() {
        let Some((pid, rest)) = line.trim_start().split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim_start();
        let call = if let Some(head) = rest.strip_suffix("<unfinished ...>") {
            pending.insert(pid, head);
            continue;
        } else if rest.starts_with("<...") {
            let (Some(head), Some((_, tail))) = (pending.remove(pid), rest.split_once("resumed>"))
            else {
                continue;
            };
            format!("{head}{tail}")
        } else {
            rest.to_string()
        };
        let Some((head, rc)) = call.trim_end().rsplit_once(" = ") else {
            continue;
        };
        let succeeded = rc
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<i64>().ok())
            .is_some_and(|n| n >= 0);
        if !succeeded {
            continue;
        }
        let Some(path) = head
            .split_once('"')
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(path, _)| Path::new(path))
        else {
            continue;
        };
        if head.starts_with("execve(") {
            // A relative path cannot be placed without the caller's
            // working directory, and is given the benefit of the doubt.
            let program = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            let elsewhere = path.is_absolute() && !ours(&program);
            if elsewhere {
                foreign.insert(pid);
            } else {
                foreign.remove(pid);
            }
            continue;
        }
        if !foreign.contains(pid) {
            found.push(path.to_path_buf());
        }
    }
    found
}

/// Run `cmd` detached from this terminal, in a process group of its own,
/// with stderr going to `stderr`, and stop the whole group after
/// `seconds`. A target that exits on its own before then is fine; one
/// that has to be stopped gets a chance to shut down cleanly first.
///
/// Whatever is left of the group once the target is done is stopped too:
/// a wrapper's children, a helper or server it started. A process that
/// left the group with `setsid` is out of reach.
///
/// Returns the exit status of a target that ended by itself unsuccessfully.
fn run_for(cmd: &mut Command, seconds: u64, stderr: File) -> io::Result<Option<ExitStatus>> {
    use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0)
        .spawn()?;
    let group = Pid::from_raw(child.id() as i32);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut failed = None;
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                failed = Some(status);
            }
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if let Some(group) = group {
        let _ = kill_process_group(group, Signal::TERM);
        let grace = Instant::now() + Duration::from_secs(2);
        while (child.try_wait()?.is_none() || test_kill_process_group(group).is_ok())
            && Instant::now() < grace
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = kill_process_group(group, Signal::KILL);
    }
    let _ = child.wait();
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_strace_call_is_joined_and_host_programs_are_left_out() {
        let log = "\
100 execve(\"/tree/bin/app\", [\"app\"], 0x1 /* 3 vars */) = 0
100 open(\"/usr/lib/libz.so.1\", O_RDONLY|O_LARGEFILE|O_CLOEXEC <unfinished ...>
101 execve(\"/host/bin/helper\", [\"helper\"], 0x2 /* 3 vars */ <unfinished ...>
100 <... open resumed>)             = 3
101 <... execve resumed>)           = 0
101 open(\"/usr/lib/libhelper.so.1\", O_RDONLY) = 3
100 open(\"/usr/lib/libmissing.so.1\", O_RDONLY) = -1 ENOENT (No such file or directory)
";
        let found = parse_strace(log, |p| p.starts_with("/tree"));
        assert_eq!(found, vec![PathBuf::from("/usr/lib/libz.so.1")]);
    }

    #[test]
    fn a_log_names_its_programs() {
        let log = "     7:\tcalling init: /usr/lib/libz.so.1\n\
                   \x20    7:\tinitialize program: /tree/bin/app\n";
        let (libs, programs) = parse_ld_debug(log);
        assert_eq!(libs, vec![PathBuf::from("/usr/lib/libz.so.1")]);
        assert_eq!(programs, vec!["/tree/bin/app"]);
    }
}
