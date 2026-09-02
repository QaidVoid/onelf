//! End-to-end pipeline tests for the recent changes:
//!
//! * store mode (`--no-compress`) round-trips byte-exact,
//! * `--preload` / `[env]` are emitted into `.onelf/`,
//! * the onelf-env constructor is injected as a DT_NEEDED and `.onelf/env`
//!   survives a sandboxed `clearenv()` + re-exec.
//!
//! These drive the real `onelf` binary (Cargo builds it for us and
//! exposes the path via `CARGO_BIN_EXE_onelf`). They need a host C
//! compiler; the DT_NEEDED / re-exec assertion additionally needs
//! `patchelf` (located via `ONELF_PATCHELF` or `PATH`). When `patchelf`
//! is absent the test instead asserts the documented fallback
//! (first-launch env still works), so it always verifies *something*
//! meaningful rather than silently passing.

use std::path::{Path, PathBuf};
use std::process::Command;

fn onelf() -> &'static str {
    env!("CARGO_BIN_EXE_onelf")
}

fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether this machine can actually mount a FUSE filesystem.
///
/// `fusermount3` being installed says nothing about whether mounting is
/// permitted. CI runners commonly ship the binary and still refuse the
/// mount, and the runtime's preferred path does not use the helper at all:
/// it unshares a mount namespace, which a hardened kernel can deny on its
/// own terms. Both have to be tried to know, so this packs a package and
/// runs it, once, forcing FUSE so a fallback to another mode cannot make
/// an unavailable mount look available.
fn fuse_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let td = workdir("fuseprobe");
        let app = td.join("app");
        std::fs::create_dir_all(app.join("bin")).unwrap();
        write(&app.join("bin/run"), "#!/bin/sh\necho FUSE_OK\n");
        std::fs::set_permissions(
            app.join("bin/run"),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();

        let pkg = td.join("probe.onelf");
        let packed = Command::new(onelf())
            .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
            .args(["--command", "bin/run", "--mtime", "0"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let ok = packed && {
            let mut run = Command::new(&pkg);
            run.env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", td.to_str().unwrap())
                .env("ONELF_MODE", "fuse");
            isolate(&mut run, &td);
            let out = run_package(&mut run);
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("FUSE_OK")
        };

        if !ok {
            eprintln!("skip: FUSE is not mountable here, as with cc and patchelf");
        }
        let _ = std::fs::remove_dir_all(&td);
        ok
    })
}

/// `patchelf` location: `ONELF_PATCHELF`, then `PATH`. `None` if absent.
fn patchelf() -> Option<String> {
    if let Ok(p) = std::env::var("ONELF_PATCHELF")
        && Path::new(&p).is_file()
    {
        return Some(p);
    }
    have("patchelf").then(|| "patchelf".to_string())
}

fn workdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "onelf-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Point a packed binary at per-test scratch for its private runtime state
/// and its extraction cache.
///
/// Without this every test shares `/tmp/onelf-<uid>` and `~/.cache/onelf`,
/// so concurrently running tests contend over one set of mountpoints and one
/// content store. `XDG_RUNTIME_DIR` is only honoured when it is `0700` and
/// owned by us, so the mode is set explicitly rather than left to the umask.
///
/// Call this after any `env_clear`, or it will be wiped again.
/// True when the packed footer carries `bit`.
fn has_footer_flag(pkg: &Path, bit: u16) -> bool {
    let data = std::fs::read(pkg).expect("read package");
    let footer = &data[data.len() - 76..];
    let flags = u16::from_le_bytes([footer[10], footer[11]]);
    flags & bit != 0
}

fn isolate(cmd: &mut Command, td: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let run = td.join("xdg-run");
    let cache = td.join("xdg-cache");
    for d in [&run, &cache] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::set_permissions(d, PermissionsExt::from_mode(0o700)).unwrap();
    }
    cmd.env("XDG_RUNTIME_DIR", &run)
        .env("XDG_CACHE_HOME", &cache);
}

/// Execute a packed binary, retrying briefly while the kernel reports the
/// file as busy.
///
/// A test writes a package and immediately execs it while sibling tests are
/// forking `onelf` subprocesses. A child forked between the write's `open`
/// and its `exec` inherits the writable descriptor, and Linux refuses to
/// execute a file that anyone holds open for writing. The window is short,
/// so a bounded retry is enough.
fn run_package(cmd: &mut Command) -> std::process::Output {
    for _ in 0..100 {
        match cmd.output() {
            Ok(out) => return out,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("run package: {e}"),
        }
    }
    panic!("package remained busy after repeated attempts")
}

fn write(path: &Path, content: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// Compile a dynamically-linked ELF with the host `cc`. Returns false if
/// no compiler is available (the only soft-skip condition).
fn cc(src: &Path, out: &Path) -> bool {
    let compiler = if have("cc") {
        "cc"
    } else if have("gcc") {
        "gcc"
    } else {
        eprintln!("skip: no C compiler available");
        return false;
    };
    let st = Command::new(compiler)
        .args(["-O0", "-o"])
        .arg(out)
        .arg(src)
        .status()
        .unwrap();
    assert!(st.success(), "compiling {} failed", src.display());
    true
}

fn run_onelf(args: &[&str], cwd: Option<&Path>) -> std::process::Output {
    let mut c = Command::new(onelf());
    c.args(args);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    c.output().expect("spawn onelf")
}

#[test]
fn store_mode_roundtrips_byte_exact() {
    let td = workdir("store");
    let app = td.join("app");
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");
    // Incompressible-ish payload so a bug that still compresses is caught
    // by the size/extract check, not masked by zstd.
    let data: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    std::fs::create_dir_all(app.join("bin")).unwrap();
    std::fs::write(app.join("bin/data.bin"), &data).unwrap();

    let pkg = td.join("s.onelf");
    let o = run_onelf(
        &[
            "pack",
            "--no-compress",
            "--command",
            "bin/run.sh",
            "--output",
            pkg.to_str().unwrap(),
            app.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "pack: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // Extract the file back and compare bytes.
    let outdir = td.join("out");
    let o = run_onelf(
        &[
            "extract",
            pkg.to_str().unwrap(),
            "--output",
            outdir.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "extract: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let got = std::fs::read(outdir.join("bin/data.bin")).unwrap();
    assert_eq!(got, data, "stored payload did not round-trip");

    // `info` reports a 1:1 ratio when stored raw.
    let o = run_onelf(&["info", pkg.to_str().unwrap()], None);
    let info = String::from_utf8_lossy(&o.stdout);
    assert!(
        info.contains("100.0%") || info.contains("ratio:       100"),
        "expected 100% ratio in `info`, got:\n{info}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

#[test]
fn preload_list_is_emitted() {
    let td = workdir("preload");
    let app = td.join("app");
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");

    let pkg = td.join("p.onelf");
    let o = run_onelf(
        &[
            "pack",
            "--command",
            "bin/run.sh",
            "--preload",
            "${ONELF_DIR}/lib/libfoo.so",
            "--preload",
            "libbar.so",
            "--output",
            pkg.to_str().unwrap(),
            app.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "pack: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let o = run_onelf(
        &[
            "extract",
            pkg.to_str().unwrap(),
            "--output",
            "-",
            "--file",
            ".onelf/preload",
        ],
        None,
    );
    assert!(o.status.success());
    let body = String::from_utf8_lossy(&o.stdout);
    assert!(body.contains("${ONELF_DIR}/lib/libfoo.so"), "got: {body:?}");
    assert!(body.contains("libbar.so"), "got: {body:?}");

    let _ = std::fs::remove_dir_all(&td);
}

/// Extraction masks setuid/setgid/sticky bits by default, and
/// `--preserve-mode` opts back in. Guards against a hostile package
/// shipping a setuid file that survives extraction.
#[test]
fn extract_masks_mode_bits_by_default() {
    use std::os::unix::fs::PermissionsExt;

    let td = workdir("mode");
    let app = td.join("app");
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    let suid = app.join("bin/suid");
    std::fs::write(&suid, b"x").unwrap();
    // 04755: setuid + rwxr-xr-x.
    std::fs::set_permissions(&suid, std::fs::Permissions::from_mode(0o4755)).unwrap();

    let pkg = td.join("m.onelf");
    let o = run_onelf(
        &[
            "pack",
            "--command",
            "bin/run.sh",
            "--output",
            pkg.to_str().unwrap(),
            app.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "pack: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // Default extraction: setuid bit stripped.
    let out = td.join("out");
    let o = run_onelf(
        &[
            "extract",
            pkg.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "extract: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let mode = std::fs::metadata(out.join("bin/suid"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o7777,
        0o755,
        "setuid bit must be stripped by default"
    );

    // --preserve-mode: setuid bit kept.
    let out2 = td.join("out2");
    let o = run_onelf(
        &[
            "extract",
            pkg.to_str().unwrap(),
            "--output",
            out2.to_str().unwrap(),
            "--preserve-mode",
        ],
        None,
    );
    assert!(
        o.status.success(),
        "extract --preserve-mode: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let mode = std::fs::metadata(out2.join("bin/suid"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o4755, "--preserve-mode must keep setuid");

    let _ = std::fs::remove_dir_all(&td);
}

/// A packed symlink whose target escapes the tree must be refused at
/// extraction, and nothing may be written outside the output dir.
#[test]
fn extract_refuses_escaping_symlink() {
    let td = workdir("evil-link");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");
    // Symlink in the source tree that points outside the package.
    std::os::unix::fs::symlink("../../../../etc/passwd", app.join("bin/evil")).unwrap();

    let pkg = td.join("e.onelf");
    let o = run_onelf(
        &[
            "pack",
            "--command",
            "bin/run.sh",
            "--output",
            pkg.to_str().unwrap(),
            app.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "pack: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let out = td.join("out");
    let o = run_onelf(
        &[
            "extract",
            pkg.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        !o.status.success(),
        "extraction of an escaping symlink must fail; stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    // The escaping symlink must not have been created.
    assert!(
        out.join("bin/evil").symlink_metadata().is_err(),
        "escaping symlink was materialized"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A corrupted manifest must produce a clean error, never a panic, from
/// the inspection commands that parse untrusted files.
#[test]
fn malformed_manifest_errors_cleanly() {
    let td = workdir("malformed");
    let app = td.join("app");
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");

    let pkg = td.join("bad.onelf");
    let o = run_onelf(
        &[
            "pack",
            "--command",
            "bin/run.sh",
            "--output",
            pkg.to_str().unwrap(),
            app.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "pack: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // Locate the manifest region via the footer and corrupt bytes inside
    // it (leaving the footer magic intact), so the parse/decompress
    // genuinely fails rather than possibly hitting the payload.
    let mut bytes = std::fs::read(&pkg).unwrap();
    let flen = bytes.len();
    let mut footer_buf = [0u8; onelf_format::FOOTER_SIZE];
    footer_buf.copy_from_slice(&bytes[flen - onelf_format::FOOTER_SIZE..]);
    let footer = onelf_format::Footer::from_bytes(&footer_buf).expect("valid footer");
    let start = footer.manifest_offset as usize;
    let end = (start + footer.manifest_compressed as usize).min(flen);
    assert!(start < end, "manifest region should be non-empty");
    for b in &mut bytes[start..end] {
        *b ^= 0xff;
    }
    std::fs::write(&pkg, &bytes).unwrap();

    let o = run_onelf(&["info", pkg.to_str().unwrap()], None);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        !stderr.contains("panicked"),
        "info panicked on a corrupt package: {stderr}"
    );
    assert!(
        !o.status.success(),
        "info must fail on a corrupt manifest; stderr: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Packing the same tree twice, in separate invocations with a fixed
/// `SOURCE_DATE_EPOCH` and a multi-key `[env]` (whose order previously
/// came from a randomized HashMap), must yield byte-identical output.
///
/// Uses a real ELF that links `libm`, so the run exercises the *bundler*
/// determinism paths too (library resolution, copies, mtime normalization,
/// onelf-env injection), not just packer-side ordering. Soft-skips when no
/// C compiler is available.
#[test]
fn build_is_byte_deterministic() {
    // Compile once to decide whether the toolchain is present; the closure
    // below recompiles per build so each runs from an independent tree.
    {
        let probe = workdir("det-probe");
        let ok = cc_libm(&probe.join("p.c"), &probe.join("p"));
        let _ = std::fs::remove_dir_all(&probe);
        if !ok {
            return; // documented soft-skip: no C compiler
        }
    }

    let build = |tag: &str| -> Vec<u8> {
        let td = workdir(tag);
        let app = td.join("app");
        std::fs::create_dir_all(app.join("bin")).unwrap();
        assert!(
            cc_libm(&td.join("prog.c"), &app.join("bin/prog")),
            "compiler vanished mid-test"
        );
        // [env] keys in deliberately non-sorted order to exercise ordering.
        write(
            &app.join("onelf.toml"),
            "[package]\nname=\"det\"\ncommand=\"bin/prog\"\n\n\
             [env]\nZULU=\"1\"\nALPHA=\"2\"\nMIKE=\"3\"\n",
        );
        let mut c = Command::new(onelf());
        c.arg("build")
            .current_dir(&app)
            .env("SOURCE_DATE_EPOCH", "1700000000");
        if let Some(pe) = patchelf() {
            c.env("ONELF_PATCHELF", pe);
        }
        let o = c.output().expect("spawn onelf build");
        assert!(
            o.status.success(),
            "build: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let bytes = std::fs::read(app.join("det.onelf")).expect("output package");
        let _ = std::fs::remove_dir_all(&td);
        bytes
    };

    let a = build("det-a");
    let b = build("det-b");
    assert_eq!(
        a,
        b,
        "two builds of the same tree must be byte-identical (len {} vs {})",
        a.len(),
        b.len()
    );
}

/// A package whose footer manifest-checksum is corrupted must fail to run
/// (the runtime verifies XXH32 over the manifest before deserializing).
#[test]
fn corrupt_manifest_checksum_fails_to_run() {
    use std::os::unix::fs::PermissionsExt;
    let td = workdir("checksum");
    let app = td.join("app");
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");

    let pkg = td.join("c.onelf");
    let o = run_onelf(
        &[
            "pack",
            "--command",
            "bin/run.sh",
            "--output",
            pkg.to_str().unwrap(),
            app.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "pack: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // Footer is the last 76 bytes; manifest_checksum sits at footer offset
    // 64..68, i.e. bytes [len-12 .. len-8]. Flip them, leaving the end
    // magic (last 8 bytes) intact so the footer still parses.
    let mut bytes = std::fs::read(&pkg).unwrap();
    let n = bytes.len();
    for b in &mut bytes[n - 12..n - 8] {
        *b ^= 0xff;
    }
    std::fs::write(&pkg, &bytes).unwrap();
    std::fs::set_permissions(&pkg, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut run = Command::new(&pkg);
    run.env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a corrupt-checksum package must not run; stderr: {stderr}"
    );
    assert!(
        stderr.contains("manifest checksum mismatch"),
        "failure must come from the checksum gate, not an unrelated error; stderr: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Compile a small ELF that links `libm` (so the packaged binary has real
/// shared-library dependencies). Returns false if no compiler is present.
fn cc_libm(src: &Path, out: &Path) -> bool {
    let compiler = if have("cc") {
        "cc"
    } else if have("gcc") {
        "gcc"
    } else {
        return false;
    };
    write(
        src,
        "#include <math.h>\n#include <stdio.h>\n\
         int main(){printf(\"%f\\n\", sqrt(2.0));return 0;}\n",
    );
    Command::new(compiler)
        .args(["-O0", "-o"])
        .arg(out)
        .arg(src)
        .arg("-lm")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The headline B2 test: with an entrypoint that gets the onelf-env
/// DT_NEEDED, `[env]` must survive the app clearing its environment and
/// re-execing itself (the sandbox scenario).
#[test]
fn env_survives_sandboxed_reexec() {
    let td = workdir("reexec");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    let result = td.join("result");

    let src = td.join("harness.c");
    write(
        &src,
        &format!(
            r#"#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <string.h>
int main(int argc, char **argv) {{
    const char *v = getenv("ONELF_IT_VAR");
    const char *d = getenv("ONELF_IT_DIR");
    if (getenv("ONELF_IT_RX")) {{
        FILE *f = fopen("{res}", "w");
        int ok = v && !strcmp(v, "survived") && d && strstr(d, "/data");
        fprintf(f, "%s v=[%s] d=[%s]\n", ok ? "PASS" : "FAIL",
                v ? v : "(null)", d ? d : "(null)");
        fclose(f);
        return ok ? 0 : 1;
    }}
    /* first launch -> wipe env, mark, re-exec self (sandbox sim) */
    clearenv();
    setenv("ONELF_IT_RX", "1", 1);
    execv("/proc/self/exe", argv);
    return 3;
}}
"#,
            res = result.display()
        ),
    );
    if !cc(&src, &app.join("bin/harness")) {
        return; // no compiler: documented soft-skip
    }
    write(
        &app.join("onelf.toml"),
        "[package]\nname=\"itest\"\ncommand=\"bin/harness\"\n\n\
         [env]\nONELF_IT_VAR=\"survived\"\nONELF_IT_DIR=\"${ONELF_DIR}/data\"\n",
    );

    // `onelf build` runs bundle-libs + pack from the recipe.
    let mut c = Command::new(onelf());
    c.arg("build").current_dir(&app);
    if let Some(pe) = patchelf() {
        c.env("ONELF_PATCHELF", pe);
    }
    let o = c.output().expect("spawn onelf build");
    let log = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(o.status.success(), "build failed:\n{log}");

    let pkg = app.join("itest.onelf");
    assert!(pkg.is_file(), "no package produced\n{log}");

    // Run the package with an intentionally minimal environment.
    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let st = run_package(&mut run).status;

    if patchelf().is_some() {
        // Full guarantee: the constructor re-applies .onelf/env after
        // the clearenv()+re-exec, so the post-re-exec process passes.
        assert!(
            log.contains("Injected onelf-env"),
            "expected onelf-env DT_NEEDED injection:\n{log}"
        );
        let r = std::fs::read_to_string(&result)
            .expect("post-re-exec process must have written the result file");
        assert!(r.starts_with("PASS"), "re-exec env not restored: {r}");
        assert!(st.success());
    } else {
        // No patchelf: pack must say so loudly and not silently ship a
        // package that claims to be re-exec-safe.
        assert!(
            log.contains("patchelf unavailable")
                || log.contains("not sandbox-re-exec-safe")
                || log.contains("re-exec-safe env"),
            "expected a fail-loud patchelf warning:\n{log}"
        );
    }

    let _ = std::fs::remove_dir_all(&td);
}

/// Identical content is compressed and stored once, and every path that
/// shares it still extracts correctly.
#[test]
fn identical_content_is_stored_once() {
    let td = workdir("dedup");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\n");

    // Content that does not compress to nothing, so the payload figure
    // reflects whether it was stored once or ten times.
    let body: Vec<u8> = (0..2_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    for i in 0..10 {
        std::fs::write(app.join(format!("copy_{i}.bin")), &body).unwrap();
    }
    std::fs::write(app.join("unique.bin"), b"only once").unwrap();

    let pkg = td.join("pkg.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0", "--level", "1"])
        .output()
        .expect("spawn onelf pack");
    assert!(
        o.status.success(),
        "pack failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // The payload is what dedup affects; the embedded runtime dominates
    // total file size and would mask the difference.
    let info = Command::new(onelf())
        .arg("info")
        .arg(&pkg)
        .output()
        .expect("spawn onelf info");
    let text = String::from_utf8_lossy(&info.stdout);
    let payload: u64 = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Size:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("could not read payload size from:\n{text}"));

    // Ten copies of a 2 MB body stored separately would exceed 20 MB.
    assert!(
        payload < 6_000_000,
        "duplicate content was not shared: payload is {payload} bytes"
    );

    let out = td.join("x");
    let o = Command::new(onelf())
        .args(["extract"])
        .arg(&pkg)
        .args(["-o"])
        .arg(&out)
        .output()
        .expect("spawn onelf extract");
    assert!(o.status.success(), "extract failed");
    for i in 0..10 {
        let got = std::fs::read(out.join(format!("copy_{i}.bin"))).unwrap();
        assert_eq!(got, body, "copy_{i} must round-trip");
    }
    assert_eq!(std::fs::read(out.join("unique.bin")).unwrap(), b"only once");

    let _ = std::fs::remove_dir_all(&td);
}

/// Packing must not hold the whole tree, so a chunk budget far below the
/// tree size has to produce exactly the same bytes as one big chunk.
#[test]
fn chunk_budget_does_not_change_output() {
    let td = workdir("chunked");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\n");
    for i in 0..12 {
        let body: Vec<u8> = (0..300_000u32)
            .map(|v| (v.wrapping_add(i) % 251) as u8)
            .collect();
        std::fs::write(app.join(format!("f{i:02}.bin")), &body).unwrap();
    }

    let pack_with = |budget: &str, out: &Path| {
        let o = Command::new(onelf())
            .args(["pack", app.to_str().unwrap(), "-o", out.to_str().unwrap()])
            .args(["--command", "bin/run", "--mtime", "0", "--level", "3"])
            .env("ONELF_PACK_CHUNK_BYTES", budget)
            .output()
            .expect("spawn onelf pack");
        assert!(
            o.status.success(),
            "pack failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    };

    let small = td.join("small.onelf");
    let big = td.join("big.onelf");
    pack_with("65536", &small);
    pack_with("999999999", &big);

    assert_eq!(
        std::fs::read(&small).unwrap(),
        std::fs::read(&big).unwrap(),
        "the chunk budget is a memory bound, not a format choice"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Owner-only modes must survive the round trip on both mount strategies.
///
/// FUSE attributes used to claim root ownership while the mount was
/// registered to the real uid, so with `default_permissions` the kernel
/// judged the owner as "other" and a `0700` binary would not run.
#[test]
fn owner_only_modes_work_under_fuse() {
    if !fuse_available() {
        return; // documented soft-skip, as with cc and patchelf
    }
    let td = workdir("ownermode");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(
        &app.join("bin/run"),
        "#!/bin/sh\ncat \"$ONELF_DIR/secret.txt\"\n\"$ONELF_DIR/bin/owner_only\"\n",
    );
    write(
        &app.join("bin/owner_only"),
        "#!/bin/sh\necho OWNER_ONLY_RAN\n",
    );
    write(&app.join("secret.txt"), "SECRET_CONTENT\n");
    let chmod = |rel: &str, mode: u32| {
        std::fs::set_permissions(
            app.join(rel),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(mode),
        )
        .unwrap()
    };
    chmod("bin/run", 0o755);
    chmod("bin/owner_only", 0o700);
    chmod("secret.txt", 0o600);

    let pkg = td.join("pkg.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    // Both strategies: the private namespace mount, and the fusermount3
    // helper that `ONELF_FUSE_NO_NAMESPACE` forces.
    for forced in [false, true] {
        let mut run = Command::new(&pkg);
        run.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", td.to_str().unwrap())
            .env("ONELF_MODE", "fuse");
        if forced {
            run.env("ONELF_FUSE_NO_NAMESPACE", "1");
        }
        isolate(&mut run, &td);
        let out = run_package(&mut run);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let which = if forced { "fusermount3" } else { "namespace" };
        assert!(
            stdout.contains("SECRET_CONTENT"),
            "0600 file unreadable on the {which} mount: {stderr}"
        );
        assert!(
            stdout.contains("OWNER_ONLY_RAN"),
            "0700 binary not executable on the {which} mount: {stderr}"
        );
    }

    let _ = std::fs::remove_dir_all(&td);
}

/// `onelf cache gc` must leave a package that a running instance still
/// holds, and reclaim one that nobody does.
///
/// The runtime pins a package with a shared lock for its lifetime; the
/// collector has to prove idleness by taking the exclusive lock rather than
/// deleting on an age check alone.
#[test]
fn cache_gc_spares_a_running_package() {
    let td = workdir("gclive");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    // Holds the package open long enough for gc to run against it.
    write(&app.join("bin/run"), "#!/bin/sh\necho STARTED\nsleep 2\n");
    std::fs::set_permissions(
        app.join("bin/run"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    let pkg = td.join("live.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    let cache = td.join("xdg-cache");
    let runtime_dir = td.join("xdg-run");
    for d in [&cache, &runtime_dir] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::set_permissions(
            d,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .unwrap();
    }

    let mut live = Command::new(&pkg);
    live.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("XDG_CACHE_HOME", &cache)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("ONELF_MODE", "cache")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match live.spawn() {
        Ok(c) => c,
        // Same ETXTBSY window the other tests hit.
        Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => return,
        Err(e) => panic!("spawn package: {e}"),
    };

    // Wait for it to announce itself, so the package is extracted and the
    // shared lock is definitely held.
    {
        use std::io::Read;
        let mut out = child.stdout.take().unwrap();
        let mut buf = [0u8; 8];
        let n = out.read(&mut buf).unwrap_or(0);
        if !String::from_utf8_lossy(&buf[..n]).contains("STARTED") {
            let mut err = String::new();
            let _ = child.stderr.take().unwrap().read_to_string(&mut err);
            let _ = child.kill();
            panic!("package did not start (read {n} bytes): {err}");
        }
    }

    let gc = |cache: &Path| -> String {
        let o = Command::new(onelf())
            .args(["cache", "gc", "--max-age", "0"])
            .env("XDG_CACHE_HOME", cache)
            .env("HOME", td.to_str().unwrap())
            .output()
            .expect("spawn onelf cache gc");
        String::from_utf8_lossy(&o.stdout).into_owned()
    };

    let while_running = gc(&cache);
    assert!(
        while_running.contains("Skipped 1"),
        "gc must spare a package a live instance holds, said: {while_running}"
    );

    let _ = child.wait();
    let when_idle = gc(&cache);
    assert!(
        when_idle.contains("Removed 1"),
        "gc must reclaim the package once idle, said: {when_idle}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Every bundled object must end up with a search path the loader will
/// actually inherit, whatever depth the executable sits at.
///
/// `DT_RUNPATH` is not consulted for a dependency's own dependencies, and an
/// object carrying one cannot inherit its parent's either, so a single one
/// anywhere in the bundle strands whatever hangs below it.
#[test]
fn bundled_objects_get_an_inheritable_search_path() {
    let td = workdir("rpath");
    let app = td.join("app");
    std::fs::create_dir_all(&app).unwrap();

    // The executable sits at the package root, the shape that resolves
    // `$ORIGIN/../lib` to a directory above the package.
    let src = td.join("m.c");
    write(
        &src,
        "#include <stdio.h>\nint main(void){puts(\"ROOT_OK\");return 0;}\n",
    );
    if !cc(&src, &app.join("app")) {
        return;
    }

    let mut c = Command::new(onelf());
    c.args(["bundle-libs", app.to_str().unwrap()]);
    if let Some(pe) = patchelf() {
        c.env("ONELF_PATCHELF", pe);
    }
    let o = c.output().expect("spawn onelf bundle-libs");
    assert!(
        o.status.success(),
        "bundle-libs failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // Nothing in the tree may keep a DT_RUNPATH, and the executable needs a
    // path that reaches the library directory beside it.
    let mut checked = 0usize;
    let mut objects = vec![app.join("app")];
    if let Ok(dir) = std::fs::read_dir(app.join("lib")) {
        objects.extend(dir.filter_map(Result::ok).map(|e| e.path()));
    }
    for obj in objects {
        let Ok(bytes) = std::fs::read(&obj) else {
            continue;
        };
        if bytes.len() < 4 || &bytes[..4] != b"\x7fELF" {
            continue;
        }
        checked += 1;
        assert!(
            !has_dynamic_tag(&bytes, DT_RUNPATH),
            "{} kept a DT_RUNPATH, which nothing below it can inherit",
            obj.display()
        );
    }
    assert!(checked > 1, "expected the executable and its libraries");
    assert!(
        has_dynamic_tag(&std::fs::read(app.join("app")).unwrap(), DT_RPATH),
        "the executable needs an inheritable search path"
    );

    let pkg = td.join("root.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "app", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("ROOT_OK"),
        "a root-level entrypoint must resolve its libraries: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&td);
}

const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;

/// Whether a 64-bit little-endian ELF carries `tag` in its dynamic section.
fn has_dynamic_tag(bytes: &[u8], tag: u64) -> bool {
    if bytes.len() < 64 || bytes[4] != 2 {
        return false;
    }
    let phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(bytes[54..56].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as usize;
    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if off + 56 > bytes.len() {
            break;
        }
        // PT_DYNAMIC
        if u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) != 2 {
            continue;
        }
        let dyn_off = u64::from_le_bytes(bytes[off + 8..off + 16].try_into().unwrap()) as usize;
        let dyn_sz = u64::from_le_bytes(bytes[off + 32..off + 40].try_into().unwrap()) as usize;
        let mut at = dyn_off;
        while at + 16 <= bytes.len() && at < dyn_off + dyn_sz {
            let t = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
            if t == 0 {
                break;
            }
            if t == tag {
                return true;
            }
            at += 16;
        }
    }
    false
}

/// The signing tooling and the runtime's verifier must agree on the
/// encodings, since both are raw fixed-width decodes with no negotiation.
///
/// This closes the publish loop end to end: generate a key, embed it at
/// pack time, sign the package, then read the key back out of the package
/// and verify the detached signature exactly as the runtime does.
#[test]
fn a_signature_verifies_against_the_key_the_package_embeds() {
    let td = workdir("signloop");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\necho SIGNED\n");
    std::fs::set_permissions(
        app.join("bin/run"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    let secret = td.join("k.key");
    let public = td.join("k.pub");
    let o = Command::new(onelf())
        .args(["key", "new"])
        .arg("--secret")
        .arg(&secret)
        .arg("--public")
        .arg(&public)
        .output()
        .expect("spawn onelf key new");
    assert!(o.status.success(), "key new failed");

    let pkg = td.join("signed.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0"])
        .args(["--update-url", "https://onelf.invalid/signed.onelf.zsync"])
        .arg("--update-key")
        .arg(&public)
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success(), "pack failed");

    let o = Command::new(onelf())
        .arg("sign")
        .arg(&pkg)
        .arg("--key")
        .arg(&secret)
        .output()
        .expect("spawn onelf sign");
    assert!(
        o.status.success(),
        "sign failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // The runtime reads the key out of the package, not off disk, so the
    // check has to come from the package too.
    // With a single --file, extract writes that file's bytes to -o directly.
    let out = td.join("embedded-key");
    let o = Command::new(onelf())
        .arg("extract")
        .arg(&pkg)
        .arg("-o")
        .arg(&out)
        .args(["--file", ".onelf/update-key"])
        .output()
        .expect("spawn onelf extract");
    assert!(
        o.status.success(),
        "extract failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let embedded = std::fs::read(&out).expect("embedded key");
    assert_eq!(embedded, std::fs::read(&public).unwrap());

    // Named for the update URL, not the binary: the runtime appends
    // `.sig` to the zsync URL it was given, so that is the only name it
    // ever requests.
    let sig_bytes = std::fs::read(td.join("signed.onelf.zsync.sig")).expect("signature");
    let pk = ed25519_compact::PublicKey::from_slice(&embedded).expect("32-byte public key");
    let sig = ed25519_compact::Signature::from_slice(&sig_bytes).expect("64-byte signature");
    assert!(
        pk.verify(std::fs::read(&pkg).unwrap(), &sig).is_ok(),
        "the runtime's decoders must accept what the signer produced"
    );

    // A published file that changed after signing must stop verifying.
    let mut tampered = std::fs::read(&pkg).unwrap();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    assert!(
        pk.verify(&tampered, &sig).is_err(),
        "a signature must not cover a modified package"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Recording where a package updates from and embedding an updater are
/// separate decisions. A package manager needs the first without paying
/// for the second, and must not have the package replace itself.
#[test]
fn an_update_url_can_be_recorded_without_embedding_an_updater() {
    let td = workdir("extupd");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\necho EXT\n");
    std::fs::set_permissions(
        app.join("bin/run"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    const URL: &str = "https://onelf.invalid/app.onelf.zsync";
    let pack_one = |out: &Path, external: bool| {
        let mut cmd = Command::new(onelf());
        cmd.args(["pack", app.to_str().unwrap(), "-o", out.to_str().unwrap()])
            .args(["--command", "bin/run", "--mtime", "0"])
            .args(["--update-url", URL]);
        if external {
            cmd.arg("--no-embed-updater");
        }
        let o = cmd.output().expect("spawn onelf pack");
        assert!(
            o.status.success(),
            "pack failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    };

    let embedded = td.join("embedded.onelf");
    let external = td.join("external.onelf");
    pack_one(&embedded, false);
    pack_one(&external, true);

    // The default must not have quietly changed: an update URL alone
    // still embeds the updater, so existing builds are unaffected.
    let embedded_len = std::fs::metadata(&embedded).unwrap().len();
    let external_len = std::fs::metadata(&external).unwrap().len();
    assert!(
        embedded_len > external_len + 1_000_000,
        "the external build should drop the updater: {embedded_len} vs {external_len}"
    );

    // Both must record the same URL, since external tooling reads it.
    for pkg in [&embedded, &external] {
        let out = td.join("url.txt");
        let o = Command::new(onelf())
            .arg("extract")
            .arg(pkg)
            .arg("-o")
            .arg(&out)
            .args(["--file", ".onelf/update-url"])
            .output()
            .expect("spawn onelf extract");
        assert!(o.status.success(), "extract failed for {}", pkg.display());
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), URL);
    }

    // And `info` must report it, since that is the supported way for an
    // external updater to find where the package comes from.
    for (pkg, expected) in [(&embedded, "embedded"), (&external, "external")] {
        let o = Command::new(onelf())
            .arg("info")
            .arg(pkg)
            .output()
            .expect("spawn onelf info");
        let stdout = String::from_utf8_lossy(&o.stdout);
        assert!(stdout.contains(URL), "info must report the update URL");
        let line = stdout
            .lines()
            .find(|l| l.trim_start().starts_with("Updater:"))
            .unwrap_or_else(|| panic!("info must report the updater for {}", pkg.display()));
        assert!(
            line.contains(expected),
            "expected {expected} updater, got: {line}"
        );
    }

    // The externally-updated package must still run, and must not act on
    // an update flag.
    let mut run = Command::new(&external);
    run.arg("--onelf-update")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("download") && !combined.contains("update available"),
        "an externally-updated package must not try to update: {combined}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A package with no update metadata must report none, rather than
/// printing an empty or placeholder value.
#[test]
fn info_reports_no_update_section_without_update_metadata() {
    let td = workdir("noupd");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\necho PLAIN\n");
    std::fs::set_permissions(
        app.join("bin/run"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    let pkg = td.join("plain.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    let o = Command::new(onelf())
        .arg("info")
        .arg(&pkg)
        .output()
        .expect("spawn onelf info");
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        !stdout.contains("Update:"),
        "a package with no update metadata must not report an update section: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Peak RSS of a child process, sampled while it runs.
///
/// `VmHWM` is monotonic, so polling can only ever under-report. That
/// matters for a test: under-sampling makes an assertion pass, never
/// fail, so this cannot produce a spurious failure.
fn peak_rss_kb(cmd: &mut Command) -> (std::process::ExitStatus, u64) {
    let mut child = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    let pid = child.id();
    let mut peak = 0u64;
    loop {
        if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("VmHWM:")
                    && let Some(kb) = v.split_whitespace().next()
                    && let Ok(kb) = kb.parse::<u64>()
                {
                    peak = peak.max(kb);
                }
            }
        }
        match child.try_wait().expect("wait") {
            Some(status) => return (status, peak),
            None => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
    }
}

/// The packer must not hold the tree it is packing.
///
/// Compression is chunked by bytes rather than by file count, so peak
/// memory tracks the chunk budget and not the input size. Guards a real
/// regression: taking the tree as one chunk took 2102 MB on a 2 GB tree
/// before this was fixed.
///
/// A tree of zeros keeps the fixture cheap (0.1 s to write, packing to
/// under 1 MB) while still making the packer move 400 MB of content.
#[test]
fn packing_does_not_hold_the_whole_tree() {
    let td = workdir("packmem");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    std::fs::create_dir_all(app.join("data")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\necho PACKED\n");
    std::fs::set_permissions(
        app.join("bin/run"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    const FILES: usize = 40;
    const PER_FILE: usize = 10 << 20;
    const TREE_BYTES: u64 = (FILES * PER_FILE) as u64;
    {
        use std::io::Write as _;
        let chunk = vec![0u8; 1 << 20];
        for i in 0..FILES {
            let f = std::fs::File::create(app.join(format!("data/f{i:03}.bin"))).unwrap();
            let mut w = std::io::BufWriter::new(f);
            for _ in 0..(PER_FILE / chunk.len()) {
                w.write_all(&chunk).unwrap();
            }
            w.flush().unwrap();
        }
    }

    let pkg = td.join("big.onelf");
    let mut cmd = Command::new(onelf());
    cmd.args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0", "--level", "1"]);
    let (status, peak) = peak_rss_kb(&mut cmd);
    assert!(status.success(), "pack failed");

    // Half the tree. Measured peak is around 90 MB against a 400 MB tree,
    // and holding the tree as one chunk measures 318 MB, so this sits
    // clear of both. Peak plateaus rather than scaling with core count
    // (47 MB at 2 threads, 102 MB at 8, 92 MB at 16), so the margin does
    // not depend on the machine.
    let limit_kb = TREE_BYTES / 1024 / 2;
    assert!(
        peak < limit_kb,
        "packing a {} MB tree peaked at {} MB, over the {} MB ceiling; \
         the packer is holding too much of the tree at once",
        TREE_BYTES / 1024 / 1024,
        peak / 1024,
        limit_kb / 1024
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A library the bundle does not provide is resolved from the host at
/// runtime, silently, into a process already holding the bundled libc.
///
/// That is the mismatch that crashes, and the publisher is the only one
/// positioned to notice, because on the packer's machine the host copy is
/// the correct one. So bundling must say which libraries will come from
/// the host.
#[test]
fn bundling_reports_libraries_it_did_not_bundle() {
    let td = workdir("hostleak");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();

    let src = td.join("m.c");
    write(
        &src,
        "#include <stdio.h>\nint main(void){puts(\"MAIN\");return 0;}\n",
    );
    if !cc(&src, &app.join("bin/main")) {
        return; // no compiler: documented soft-skip
    }

    // A complete bundle has nothing to report.
    let o = Command::new(onelf())
        .args(["bundle-libs", app.to_str().unwrap()])
        .output()
        .expect("spawn bundle-libs");
    assert!(o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        !err.contains("not in the bundle"),
        "a complete bundle must not warn: {err}"
    );

    // Excluding a real dependency is the same situation a dlopen-only or
    // unresolvable library produces, and must be named.
    let app2 = td.join("app2");
    std::fs::create_dir_all(app2.join("bin")).unwrap();
    std::fs::copy(app.join("bin/main"), app2.join("bin/main")).unwrap();
    let o = Command::new(onelf())
        .args(["bundle-libs", app2.to_str().unwrap()])
        .args(["--exclude", "libc.so.6"])
        .output()
        .expect("spawn bundle-libs");
    assert!(o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("not in the bundle") && err.contains("libc.so.6"),
        "an excluded dependency must be named as host-resolved: {err}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// The host's library directories are on the search path so GPU drivers
/// stay reachable, but they hold every system library, so a package that
/// needs nothing from the host should not have them.
#[test]
fn a_package_needing_nothing_from_the_host_does_not_get_its_lib_dirs() {
    let td = workdir("hostlibs");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();

    let src = td.join("m.c");
    write(
        &src,
        "#include <stdio.h>\nint main(void){puts(\"MAIN\");return 0;}\n",
    );
    if !cc(&src, &app.join("bin/main")) {
        return; // no compiler: documented soft-skip
    }
    let o = Command::new(onelf())
        .args(["bundle-libs", app.to_str().unwrap()])
        .output()
        .expect("spawn bundle-libs");
    assert!(o.status.success());

    let pack_as = |name: &str, mode: Option<&str>| {
        let out = td.join(name);
        let mut cmd = Command::new(onelf());
        cmd.args(["pack", app.to_str().unwrap(), "-o", out.to_str().unwrap()])
            .args(["--command", "bin/main", "--mtime", "0"]);
        if let Some(m) = mode {
            cmd.args(["--host-libs", m]);
        }
        let o = cmd.output().expect("spawn onelf pack");
        assert!(
            o.status.success(),
            "pack failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        out
    };

    // A plain CLI app references no driver stack, so `auto` withholds them.
    let auto = pack_as("auto.onelf", None);
    assert!(
        has_footer_flag(&auto, 1 << 5),
        "auto must withhold host lib dirs from a package that needs none"
    );

    // And the package still runs.
    let mut run = Command::new(&auto);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "MAIN");

    // The override is honoured in both directions.
    assert!(
        !has_footer_flag(&pack_as("always.onelf", Some("always")), 1 << 5),
        "--host-libs always must expose them"
    );
    assert!(
        has_footer_flag(&pack_as("never.onelf", Some("never")), 1 << 5),
        "--host-libs never must withhold them"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A package that loads a driver stack keeps the host directories, since
/// GPU userspace has to come from the host.
#[test]
fn a_package_that_uses_a_driver_stack_is_packed_with_auto() {
    const NO_HOST_LIB_DIRS: u16 = 1 << 5;
    const HOST_LIBS_ALWAYS: u16 = 1 << 6;

    let td = workdir("hostlibsgl");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();

    let src = td.join("g.c");
    // dlopen'd by name, exactly how a real driver stack is reached: the
    // soname never appears in DT_NEEDED, only as a string in the binary.
    write(
        &src,
        "#include <stdio.h>\nconst char *drv = \"libvulkan.so.1\";\n         int main(void){puts(drv);return 0;}\n",
    );
    if !cc(&src, &app.join("bin/g")) {
        return; // no compiler: documented soft-skip
    }

    let pkg = td.join("gl.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/g", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());
    assert!(
        !has_footer_flag(&pkg, NO_HOST_LIB_DIRS) && !has_footer_flag(&pkg, HOST_LIBS_ALWAYS),
        "a package referencing a driver soname must be packed with auto"
    );
    let info = Command::new(onelf())
        .args(["info", pkg.to_str().unwrap()])
        .output()
        .expect("spawn onelf info");
    assert!(String::from_utf8_lossy(&info.stdout).contains("Host libs:      auto"));

    // The same binary with no driver reference takes nothing from the host.
    write(
        &src,
        "#include <stdio.h>\nint main(void){puts(\"x\");return 0;}\n",
    );
    assert!(cc(&src, &app.join("bin/g")));
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/g", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());
    assert!(has_footer_flag(&pkg, NO_HOST_LIB_DIRS));

    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args([
            "--command",
            "bin/g",
            "--mtime",
            "0",
            "--host-libs",
            "always",
        ])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());
    assert!(has_footer_flag(&pkg, HOST_LIBS_ALWAYS));
    assert!(!has_footer_flag(&pkg, NO_HOST_LIB_DIRS));

    let _ = std::fs::remove_dir_all(&td);
}

/// Compile with the host `cc` and explicit arguments. Returns false when
/// no compiler is available.
fn cc_with(args: &[&str], out: &Path) -> bool {
    let compiler = if have("cc") {
        "cc"
    } else if have("gcc") {
        "gcc"
    } else {
        eprintln!("skip: no C compiler available");
        return false;
    };
    let st = Command::new(compiler)
        .arg("-o")
        .arg(out)
        .args(args)
        .status()
        .unwrap();
    assert!(st.success(), "compiling {} failed", out.display());
    true
}

/// A new-format glibc loader cache image naming `paths`, so a test can
/// describe a host of its own making through `ONELF_LD_CACHE`.
fn ld_so_cache(paths: &[&Path]) -> Vec<u8> {
    let mut out = Vec::from(&b"glibc-ld.so.cache1.1"[..]);
    out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&[0; 3]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[0; 12]);
    let strings_at = 48 + paths.len() * 24;
    let mut strings: Vec<u8> = Vec::new();
    for p in paths {
        let off = (strings_at + strings.len()) as u32;
        out.extend_from_slice(&0i32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        strings.extend_from_slice(p.to_str().unwrap().as_bytes());
        strings.push(0);
    }
    out.extend_from_slice(&strings);
    out
}

/// The crash class the resolver exists for: a host driver built against a
/// newer copy of a library the bundle also carries. Directory ordering
/// loads the bundled copy first and the driver fails to bind. The resolver
/// compares the two by symbol version and hands the driver the host copy.
///
/// The host is a fixture: a driver `libGL.so.1` needing `FIX_2.0` from
/// `libfixture.so.1`, described by a loader cache of its own.
#[test]
fn the_resolver_hands_a_host_driver_its_newer_dependency() {
    let td = workdir("resolver");
    let app = td.join("app");
    let host = td.join("host");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    std::fs::create_dir_all(app.join("lib")).unwrap();
    std::fs::create_dir_all(&host).unwrap();

    let fixture_c = td.join("fixture.c");
    write(
        &fixture_c,
        "int fix_value(void){return 1;}\nint fix_extra(void){return 2;}\n",
    );
    let old_map = td.join("old.map");
    write(&old_map, "FIX_1.0 { global: fix_value; local: *; };\n");
    let new_map = td.join("new.map");
    write(
        &new_map,
        "FIX_1.0 { global: fix_value; local: *; };\nFIX_2.0 { global: fix_extra; } FIX_1.0;\n",
    );
    let bundled_fixture = app.join("lib/libfixture.so.1");
    if !cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libfixture.so.1",
            &format!("-Wl,--version-script={}", old_map.display()),
            fixture_c.to_str().unwrap(),
        ],
        &bundled_fixture,
    ) {
        return; // no compiler: documented soft-skip
    }
    let host_fixture = host.join("libfixture.so.1");
    assert!(cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libfixture.so.1",
            &format!("-Wl,--version-script={}", new_map.display()),
            fixture_c.to_str().unwrap(),
        ],
        &host_fixture,
    ));

    let gl_c = td.join("gl.c");
    write(
        &gl_c,
        "int fix_extra(void);\nint gl_probe(void){return fix_extra();}\n",
    );
    let host_gl = host.join("libGL.so.1");
    assert!(cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libGL.so.1",
            gl_c.to_str().unwrap(),
            &format!("-L{}", host.display()),
            "-l:libfixture.so.1",
        ],
        &host_gl,
    ));

    let app_c = td.join("app.c");
    write(
        &app_c,
        r#"#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
int main(void) {
    const char *lp = getenv("LD_LIBRARY_PATH");
    printf("LD_LIBRARY_PATH=%s\n", lp ? lp : "");
    void *h = dlopen("libGL.so.1", RTLD_NOW);
    if (!h) { printf("dlopen: %s\n", dlerror()); return 1; }
    int (*probe)(void) = dlsym(h, "gl_probe");
    if (!probe) { printf("dlsym: %s\n", dlerror()); return 1; }
    printf("probe=%d\n", probe());
    return 0;
}
"#,
    );
    assert!(cc_with(
        &[app_c.to_str().unwrap(), "-ldl"],
        &app.join("bin/app")
    ));

    let cache = td.join("ld.so.cache");
    std::fs::write(&cache, ld_so_cache(&[&host_gl, &host_fixture])).unwrap();

    let pkg = td.join("app.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/app", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(
        o.status.success(),
        "pack failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        !has_footer_flag(&pkg, 1 << 5),
        "the driver reference selects auto"
    );

    let launch = |no_resolver: bool| {
        let mut run = Command::new(&pkg);
        run.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", td.to_str().unwrap())
            .env("ONELF_LD_CACHE", &cache);
        if no_resolver {
            run.env("ONELF_NO_RESOLVER", "1");
        }
        isolate(&mut run, &td);
        run_package(&mut run)
    };

    let without = launch(true);
    assert!(
        !without.status.success(),
        "without the resolver the driver must fail to bind: {}",
        String::from_utf8_lossy(&without.stdout)
    );

    let with = launch(false);
    let stdout = String::from_utf8_lossy(&with.stdout);
    assert!(
        with.status.success() && stdout.contains("probe=2"),
        "with the resolver the host driver binds against the host copy:\n{stdout}{}",
        String::from_utf8_lossy(&with.stderr)
    );
    let lp = stdout
        .lines()
        .find_map(|l| l.strip_prefix("LD_LIBRARY_PATH="))
        .expect("the app reports its library path");
    let private = td.join("xdg-run");
    for dir in lp.split(':').filter(|d| !d.is_empty()) {
        assert!(
            Path::new(dir).starts_with(&private),
            "a host directory reached the library path: {dir}"
        );
    }

    let _ = std::fs::remove_dir_all(&td);
}

/// Two copies that each define a version the other lacks cannot be ordered.
/// The bundled copy is kept and the soname is named on stderr.
#[test]
fn incomparable_copies_are_named_and_stay_bundled() {
    let td = workdir("incomparable");
    let app = td.join("app");
    let host = td.join("host");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    std::fs::create_dir_all(app.join("lib")).unwrap();
    std::fs::create_dir_all(&host).unwrap();

    let fixture_c = td.join("fixture.c");
    write(
        &fixture_c,
        "int fix_value(void){return 1;}\nint fix_extra(void){return 2;}\n",
    );
    let bundle_map = td.join("bundle.map");
    write(
        &bundle_map,
        "FIX_1.0 { global: fix_value; local: *; };\nFIX_2.0 { global: fix_extra; } FIX_1.0;\n",
    );
    let host_map = td.join("host.map");
    write(
        &host_map,
        "FIX_1.0 { global: fix_value; local: *; };\nVENDOR_1 { global: fix_extra; } FIX_1.0;\n",
    );
    if !cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libfixture.so.1",
            &format!("-Wl,--version-script={}", bundle_map.display()),
            fixture_c.to_str().unwrap(),
        ],
        &app.join("lib/libfixture.so.1"),
    ) {
        return; // no compiler: documented soft-skip
    }
    let host_fixture = host.join("libfixture.so.1");
    assert!(cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libfixture.so.1",
            &format!("-Wl,--version-script={}", host_map.display()),
            fixture_c.to_str().unwrap(),
        ],
        &host_fixture,
    ));
    let gl_c = td.join("gl.c");
    write(&gl_c, "int gl_probe(void){return 7;}\n");
    let host_gl = host.join("libGL.so.1");
    assert!(cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libGL.so.1",
            gl_c.to_str().unwrap(),
            &format!("-L{}", host.display()),
            "-l:libfixture.so.1",
        ],
        &host_gl,
    ));
    let app_c = td.join("app.c");
    write(
        &app_c,
        "#include <stdio.h>\nconst char *drv = \"libGL.so.1\";\nint main(void){puts(drv);return 0;}\n",
    );
    assert!(cc_with(&[app_c.to_str().unwrap()], &app.join("bin/app")));

    let cache = td.join("ld.so.cache");
    std::fs::write(&cache, ld_so_cache(&[&host_gl, &host_fixture])).unwrap();

    let pkg = td.join("app.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/app", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("ONELF_LD_CACHE", &cache);
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("onelf-rt: resolver: cannot order libfixture.so.1"),
        "the incomparable soname must be named:\n{stderr}"
    );
    let farm = std::fs::read_dir(td.join("xdg-run"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("resolve-"))
        })
        .map(|p| p.join("farm"));
    let farm = farm.expect("the resolver records its decision");
    assert!(
        farm.join("libGL.so.1").is_symlink(),
        "the driver comes from the host"
    );
    assert!(
        !farm.join("libfixture.so.1").exists(),
        "an incomparable copy stays bundled"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Unlink the version definition before the last one from the ELF at
/// `path`, so the copy reads as defining fewer versions than the one it
/// was made from while staying the same bytes everywhere else. glibc lists
/// its private version last, and the loader needs that one, so the entry
/// before it is the newest public version.
fn trim_newest_version(path: &Path) {
    let mut data = std::fs::read(path).unwrap();
    assert_eq!(&data[0..4], b"\x7fELF");
    assert_eq!(data[4], 2, "64-bit ELF expected");
    let shoff = u64::from_le_bytes(data[40..48].try_into().unwrap()) as usize;
    let shentsize = u16::from_le_bytes(data[58..60].try_into().unwrap()) as usize;
    let shnum = u16::from_le_bytes(data[60..62].try_into().unwrap()) as usize;
    let mut verdef = None;
    for i in 0..shnum {
        let at = shoff + i * shentsize;
        let sh_type = u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap());
        if sh_type == 0x6fff_fffd {
            let offset = u64::from_le_bytes(data[at + 24..at + 32].try_into().unwrap()) as usize;
            verdef = Some(offset);
            break;
        }
    }
    let base = verdef.expect("the library defines versions");
    let mut positions = vec![base];
    loop {
        let pos = *positions.last().unwrap();
        let next = u32::from_le_bytes(data[pos + 16..pos + 20].try_into().unwrap()) as usize;
        if next == 0 {
            break;
        }
        positions.push(pos + next);
    }
    assert!(positions.len() >= 3, "at least three version definitions");
    let last = positions[positions.len() - 1];
    let before = positions[positions.len() - 3];
    let skip = (last - before) as u32;
    data[before + 16..before + 20].copy_from_slice(&skip.to_le_bytes());
    std::fs::write(path, data).unwrap();
}

/// The path `ldd` reports for `soname` when running `binary`.
fn ldd_path(binary: &Path, soname: &str) -> Option<PathBuf> {
    let out = Command::new("ldd").arg(binary).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with(soname) || line.contains(&format!("/{soname}")) {
            let path = line
                .split_once("=> ")
                .map(|(_, rest)| rest)
                .unwrap_or(line)
                .split_whitespace()
                .next()?;
            return Some(PathBuf::from(path));
        }
        None
    })
}

/// A host whose glibc is newer than the bundle's runs the entrypoint under
/// the host loader, with the bundle's own libraries still loading from the
/// bundle.
///
/// This machine's glibc is both sides of the comparison: the bundled copy
/// is the same file with its newest version definition trimmed away, so
/// the host copy reads as a strict superset.
#[test]
fn a_newer_host_glibc_runs_the_entrypoint_under_the_host_loader() {
    let td = workdir("hostloader");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();

    let fixture_c = td.join("fixture.c");
    write(&fixture_c, "int fix_value(void){return 41;}\n");
    let fixture = td.join("libfixture.so.1");
    if !cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libfixture.so.1",
            fixture_c.to_str().unwrap(),
        ],
        &fixture,
    ) {
        return; // no compiler: documented soft-skip
    }
    let app_c = td.join("app.c");
    write(
        &app_c,
        r#"#include <stdio.h>
#include <stdlib.h>
int fix_value(void);
int main(void) {
    FILE *m = fopen("/proc/self/maps", "r");
    char line[4096];
    while (m && fgets(line, sizeof line, m)) fputs(line, stdout);
    printf("value=%d\n", fix_value() + 1);
    return 0;
}
"#,
    );
    assert!(cc_with(
        &[
            app_c.to_str().unwrap(),
            &format!("-L{}", td.display()),
            "-l:libfixture.so.1",
        ],
        &app.join("bin/app"),
    ));
    let Some(host_libc) = ldd_path(&app.join("bin/app"), "libc.so.6") else {
        eprintln!("skip: ldd does not report libc.so.6");
        return;
    };
    let Some(host_ld) = ldd_path(&app.join("bin/app"), "ld-linux") else {
        eprintln!("skip: ldd does not report the loader");
        return;
    };

    let o = Command::new(onelf())
        .args(["bundle-libs", app.to_str().unwrap()])
        .args(["--search-path", td.to_str().unwrap()])
        .output()
        .expect("spawn onelf bundle-libs");
    assert!(
        o.status.success(),
        "bundle-libs failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let bundled_libc = app.join("lib/libc.so.6");
    assert!(bundled_libc.is_file(), "bundle-libs bundles libc");
    trim_newest_version(&bundled_libc);

    let cache = td.join("ld.so.cache");
    let mut listed: Vec<PathBuf> = vec![host_libc.clone(), host_ld.clone()];
    for name in ["libm.so.6", "libdl.so.2", "libpthread.so.0", "librt.so.1"] {
        if let Some(p) = ldd_path(&app.join("bin/app"), name) {
            listed.push(p);
        }
        let sibling = host_libc.with_file_name(name);
        if sibling.is_file() && !listed.contains(&sibling) {
            listed.push(sibling);
        }
    }
    let refs: Vec<&Path> = listed.iter().map(PathBuf::as_path).collect();
    std::fs::write(&cache, ld_so_cache(&refs)).unwrap();

    let pkg = td.join("app.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args([
            "--command",
            "bin/app",
            "--mtime",
            "0",
            "--host-libs",
            "always",
        ])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    let launch = |no_resolver: bool| {
        let mut run = Command::new(&pkg);
        run.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", td.to_str().unwrap())
            .env("ONELF_LD_CACHE", &cache);
        if no_resolver {
            run.env("ONELF_NO_RESOLVER", "1");
        }
        isolate(&mut run, &td);
        run_package(&mut run)
    };

    // A host file shadowing a bundled one keeps the bundled path, so the
    // mapping is identified by inode rather than by name.
    let inode_of = |p: &Path| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p).unwrap().ino()
    };
    let host_libc_ino = inode_of(&host_libc);
    let host_ld_ino = inode_of(&host_ld);
    let mapped_inodes = |maps: &str, soname: &str| -> Vec<u64> {
        let mut inodes: Vec<u64> = maps
            .lines()
            .filter(|l| l.ends_with(soname) || l.contains(&format!("/{soname} ")))
            .filter_map(|l| l.split_whitespace().nth(4)?.parse().ok())
            .collect();
        inodes.sort();
        inodes.dedup();
        inodes
    };
    let maps_of = |out: &std::process::Output| String::from_utf8_lossy(&out.stdout).into_owned();

    let without = launch(true);
    let maps = maps_of(&without);
    assert!(
        without.status.success() && maps.contains("value=42"),
        "the bundle runs on its own:\n{maps}{}",
        String::from_utf8_lossy(&without.stderr)
    );
    assert!(
        !mapped_inodes(&maps, "libc.so.6").contains(&host_libc_ino),
        "without the resolver the bundled libc is mapped:\n{maps}"
    );

    let with = launch(false);
    let maps = maps_of(&with);
    assert!(
        with.status.success() && maps.contains("value=42"),
        "the entrypoint runs under the host loader:\n{maps}{}",
        String::from_utf8_lossy(&with.stderr)
    );
    assert_eq!(
        mapped_inodes(&maps, "ld-linux-x86-64.so.2"),
        [host_ld_ino],
        "only the host loader is mapped:\n{maps}"
    );
    assert_eq!(
        mapped_inodes(&maps, "libc.so.6"),
        [host_libc_ino],
        "only the host libc is mapped:\n{maps}"
    );
    assert!(
        maps.contains("libfixture.so.1"),
        "the bundle's own library still loads:\n{maps}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// With nothing taken from the host, a soname the bundle lacks fails by
/// name rather than being satisfied by a host copy.
#[test]
fn a_never_package_fails_by_soname_when_a_library_is_missing() {
    let td = workdir("neverfail");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    std::fs::create_dir_all(app.join("lib")).unwrap();

    let fixture_c = td.join("fixture.c");
    write(&fixture_c, "int fix_value(void){return 1;}\n");
    let fixture = td.join("libfixture.so.1");
    if !cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libfixture.so.1",
            fixture_c.to_str().unwrap(),
        ],
        &fixture,
    ) {
        return; // no compiler: documented soft-skip
    }
    let app_c = td.join("app.c");
    write(
        &app_c,
        "#include <stdio.h>\nint fix_value(void);\nint main(void){printf(\"%d\\n\", fix_value());return 0;}\n",
    );
    assert!(cc_with(
        &[
            app_c.to_str().unwrap(),
            &format!("-L{}", td.display()),
            "-l:libfixture.so.1",
        ],
        &app.join("bin/app"),
    ));

    // Bundle the loader and libc so the launch never consults the host
    // loader's own search path, then withhold the one library the app
    // needs.
    let o = Command::new(onelf())
        .args(["bundle-libs", app.to_str().unwrap()])
        .args(["--search-path", td.to_str().unwrap()])
        .output()
        .expect("spawn onelf bundle-libs");
    assert!(
        o.status.success(),
        "bundle-libs failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(app.join("lib/libfixture.so.1").is_file());
    std::fs::remove_file(app.join("lib/libfixture.so.1")).unwrap();

    let pkg = td.join("app.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args([
            "--command",
            "bin/app",
            "--mtime",
            "0",
            "--host-libs",
            "never",
        ])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("libfixture.so.1"),
        "the missing soname must be named:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// FUSE must stream an entry, not buffer it.
///
/// The proof is an address-space limit well below the entry size: if any
/// stage held the whole entry, the allocation would fail. A 300 MB entry
/// of zeros packs to under 1 MB, so this costs the suite a fraction of a
/// second rather than a large fixture.
#[test]
fn a_large_entry_reads_under_an_address_space_limit() {
    if !fuse_available() {
        return; // documented soft-skip, as with cc and patchelf
    }
    let td = workdir("fusemem");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(
        &app.join("bin/run"),
        // `cat` into `wc`, deliberately: `wc -c < file` fstats the file
        // and never reads a byte, so it would pass without touching FUSE.
        "#!/bin/sh\ncat \"$ONELF_DIR/big.bin\" | wc -c\n",
    );
    std::fs::set_permissions(
        app.join("bin/run"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    // Written in chunks so the test process does not hold it either.
    const ENTRY_BYTES: usize = 300_000_000;
    {
        use std::io::Write as _;
        let f = std::fs::File::create(app.join("big.bin")).unwrap();
        let mut w = std::io::BufWriter::new(f);
        let chunk = vec![0u8; 1 << 20];
        let mut written = 0;
        while written < ENTRY_BYTES {
            let n = chunk.len().min(ENTRY_BYTES - written);
            w.write_all(&chunk[..n]).unwrap();
            written += n;
        }
        w.flush().unwrap();
    }

    let pkg = td.join("big.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0", "--level", "1"])
        .output()
        .expect("spawn onelf pack");
    assert!(
        o.status.success(),
        "pack failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // 128 MiB of address space against a 300 MB entry. Buffering the
    // entry, or decompressing it whole, cannot fit.
    const AS_LIMIT_KB: usize = 128 * 1024;
    let mut run = Command::new("/bin/sh");
    run.arg("-c")
        .arg(format!(
            "ulimit -v {AS_LIMIT_KB}; exec {}",
            pkg.to_str().unwrap()
        ))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("ONELF_MODE", "fuse");
    isolate(&mut run, &td);
    let out = run_package(&mut run);

    assert!(
        out.status.success(),
        "reading under a {} MiB limit failed: {}",
        AS_LIMIT_KB / 1024,
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        ENTRY_BYTES.to_string(),
        "the whole entry must be readable: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A package that names an update URL but carries no signing key must
/// refuse to update, and must not reach the network to find that out.
#[test]
fn self_update_refuses_without_a_signing_key() {
    let td = workdir("nokey");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\necho APP\n");
    std::fs::set_permissions(
        app.join("bin/run"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    let pkg = td.join("nokey.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0"])
        // A host that cannot resolve, so a request would say so distinctly.
        .args(["--update-url", "https://onelf.invalid/app.zsync"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    for flag in ["--onelf-update", "--onelf-check-update"] {
        let mut run = Command::new(&pkg);
        run.arg(flag)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", td.to_str().unwrap());
        isolate(&mut run, &td);
        let out = run_package(&mut run);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("no signing key"),
            "{flag} must refuse an unsigned package, got: {err}"
        );
        assert!(
            !err.contains("resolve") && !err.contains("dns") && !err.contains("connect"),
            "{flag} must refuse before reaching the network, got: {err}"
        );
    }

    let _ = std::fs::remove_dir_all(&td);
}

/// Several instances starting at once must each get a complete package.
///
/// Extraction publishes through a rename and a completion marker, so a
/// second runner either waits or sees a finished tree, never a partial one.
#[test]
fn concurrent_first_runs_never_see_a_partial_package() {
    let td = workdir("race");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(
        &app.join("bin/run"),
        "#!/bin/sh\ncat \"$ONELF_DIR/payload.bin\" | wc -c\n",
    );
    std::fs::set_permissions(
        app.join("bin/run"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();
    // Big enough that extraction takes long enough for the runs to overlap.
    let payload: Vec<u8> = (0..8_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(app.join("payload.bin"), &payload).unwrap();

    let pkg = td.join("race.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0", "--level", "1"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    let mut kids = Vec::new();
    for _ in 0..6 {
        let mut run = Command::new(&pkg);
        run.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", td.to_str().unwrap())
            .env("ONELF_MODE", "cache");
        // One cache for the whole test, so the six contend over one extraction.
        isolate(&mut run, &td);
        run.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        match run.spawn() {
            Ok(c) => kids.push(c),
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => return,
            Err(e) => panic!("spawn package: {e}"),
        }
    }

    let expected = format!("{}", payload.len());
    for (i, kid) in kids.into_iter().enumerate() {
        let out = kid.wait_with_output().expect("wait");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "run {i} failed: {stderr}");
        assert_eq!(
            stdout.trim(),
            expected,
            "run {i} saw an incomplete payload: {stderr}"
        );
    }

    let _ = std::fs::remove_dir_all(&td);
}

/// With no safe cache root available, every cache subcommand must refuse
/// rather than fall back to a shared world-writable path. `onelf cache
/// clear` in particular used to be able to `remove_dir_all` `/tmp/onelf`.
#[test]
fn cache_commands_refuse_without_a_safe_root() {
    let td = workdir("nosafe");
    for sub in [
        vec!["cache", "list"],
        vec!["cache", "gc", "--max-age", "0"],
        vec!["cache", "clear"],
    ] {
        let o = Command::new(onelf())
            .args(&sub)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap_or_else(|e| panic!("spawn onelf {sub:?}: {e}"));
        assert_eq!(
            o.status.code(),
            Some(1),
            "`onelf {}` must refuse without a safe root",
            sub.join(" ")
        );
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("no safe cache directory"),
            "`onelf {}` must say why, got: {err}",
            sub.join(" ")
        );
    }
    let _ = std::fs::remove_dir_all(&td);
}

/// The recorded interpreter must come from the entrypoint, not from
/// whichever ELF the sorted walk reaches first. `bin/helper` sorts before
/// `bin/main`, which is what used to give it the casting vote.
#[test]
fn recorded_interpreter_comes_from_the_entrypoint() {
    let td = workdir("interp");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    std::fs::create_dir_all(app.join("lib")).unwrap();

    let src = td.join("m.c");
    write(&src, "int main(void){return 0;}\n");
    if !cc(&src, &app.join("bin/main")) {
        return;
    }
    let mut bytes = std::fs::read(app.join("bin/main")).unwrap();
    let glibc = b"/lib64/ld-linux-x86-64.so.2\0";
    let musl = b"/lib/ld-musl-x86_64.so.1\0";
    let Some(at) = bytes
        .windows(glibc.len())
        .position(|w| w == glibc.as_slice())
    else {
        return; // unexpected host interpreter
    };
    bytes[at..at + musl.len()].copy_from_slice(musl);
    for b in &mut bytes[at + musl.len()..at + glibc.len()] {
        *b = 0;
    }
    std::fs::write(app.join("bin/helper"), &bytes).unwrap();

    // Both loaders present, so picking the wrong entrypoint records the
    // wrong one rather than recording nothing.
    write(&app.join("lib/ld-linux-x86-64.so.2"), "not a real loader\n");
    write(&app.join("lib/ld-musl-x86_64.so.1"), "not a real loader\n");

    let pkg = td.join("pkg.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/main", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(
        o.status.success(),
        "pack failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let o = Command::new(onelf())
        .args(["extract"])
        .arg(&pkg)
        .args(["-o", "-", "--file", ".onelf/interp"])
        .output()
        .expect("spawn onelf extract");
    let recorded = String::from_utf8_lossy(&o.stdout).trim().to_string();
    assert_eq!(
        recorded, "lib/ld-linux-x86-64.so.2",
        "the entrypoint is glibc, so its loader is the one to record"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A helper built against another libc must not decide what gets bundled.
///
/// Architecture and libc came from whichever path sorted first, so adding a
/// musl helper to a glibc tree made the bundler drop the entrypoint's own
/// loader and bundle nothing, leaving a package that only ran where glibc
/// already existed.
#[test]
fn a_foreign_libc_helper_does_not_hijack_bundling() {
    let td = workdir("mixedlibc");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();

    let src = td.join("m.c");
    write(
        &src,
        "#include <stdio.h>\nint main(void){puts(\"MAIN\");return 0;}\n",
    );
    if !cc(&src, &app.join("bin/main")) {
        return; // no compiler: documented soft-skip
    }

    // A real, parseable ELF that claims a musl interpreter, made by
    // rewriting the interp string of a copy. Sorts before `main`, which is
    // what used to give it the casting vote.
    let mut bytes = std::fs::read(app.join("bin/main")).unwrap();
    let musl = b"/lib/ld-musl-x86_64.so.1\0";
    let glibc = b"/lib64/ld-linux-x86-64.so.2\0";
    let Some(at) = bytes
        .windows(glibc.len())
        .position(|w| w == glibc.as_slice())
    else {
        return; // unexpected host interpreter; nothing to rewrite
    };
    bytes[at..at + musl.len()].copy_from_slice(musl);
    for b in &mut bytes[at + musl.len()..at + glibc.len()] {
        *b = 0;
    }
    std::fs::write(app.join("bin/helper"), &bytes).unwrap();

    write(
        &app.join("onelf.toml"),
        "[package]\nname=\"mixedlibc\"\ncommand=\"bin/main\"\n",
    );

    let mut c = Command::new(onelf());
    c.arg("build").current_dir(&app);
    if let Some(pe) = patchelf() {
        c.env("ONELF_PATCHELF", pe);
    }
    let o = c.output().expect("spawn onelf build");
    let log = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(o.status.success(), "build failed:\n{log}");

    // The entrypoint's own loader has to be there, or the package only
    // runs where its libc already exists.
    let lib = app.join("lib");
    let bundled: Vec<String> = std::fs::read_dir(&lib)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        bundled.iter().any(|n| n.starts_with("ld-linux")),
        "the entrypoint's loader must be bundled, got: {bundled:?}\n{log}"
    );
    assert!(
        log.contains("mixes libc families"),
        "a mixed tree must say which family won:\n{log}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A partially downloaded package must report why rather than abort. Two
/// shapes matter: the footer missing entirely, and a footer that survived
/// while the body it describes did not.
#[test]
fn truncated_packages_report_rather_than_abort() {
    let td = workdir("truncated");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\n");
    let filler: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(app.join("data.bin"), &filler).unwrap();

    let full = td.join("full.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", full.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());
    let bytes = std::fs::read(&full).unwrap();

    // Cut short: the footer lives at the end, so it goes with the tail.
    let headless = td.join("headless.onelf");
    std::fs::write(&headless, &bytes[..bytes.len() / 2]).unwrap();

    // Footer preserved over a shortened body, so the regions it names can
    // no longer be backed by the file.
    let gutted = td.join("gutted.onelf");
    let mut g = bytes[..bytes.len() / 2].to_vec();
    g.extend_from_slice(&bytes[bytes.len() - 76..]);
    std::fs::write(&gutted, &g).unwrap();

    for pkg in [&headless, &gutted] {
        for cmd in ["info", "list", "verify"] {
            let o = Command::new(onelf())
                .arg(cmd)
                .arg(pkg)
                .output()
                .unwrap_or_else(|e| panic!("spawn onelf {cmd}: {e}"));
            assert_eq!(
                o.status.code(),
                Some(1),
                "`onelf {cmd}` on {} must exit with an error, not die",
                pkg.display()
            );
            let err = String::from_utf8_lossy(&o.stderr);
            assert!(
                err.contains("magic") || err.contains("out of bounds"),
                "`onelf {cmd}` must say what is wrong, got: {err}"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&td);
}

/// Serving a file must still refuse tampered content now that the check is
/// per block rather than per entry, and reads of untouched blocks in the
/// same file must keep working.
#[test]
fn a_tampered_block_is_refused_but_neighbours_still_read() {
    let td = workdir("blocktamper");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\ncat data\n");

    // Several 256 KiB blocks, each filled distinctly so a corrupted one is
    // identifiable in the output.
    let mut data = Vec::new();
    for i in 0..4u8 {
        data.extend(std::iter::repeat_n(b'a' + i, 256 * 1024));
    }
    std::fs::write(app.join("data"), &data).unwrap();

    let pkg = td.join("pkg.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0", "--no-compress"])
        .output()
        .expect("spawn onelf pack");
    assert!(
        o.status.success(),
        "pack failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // Store mode puts the content in the payload verbatim, so the last
    // block's bytes can be found and altered directly.
    let mut bytes = std::fs::read(&pkg).unwrap();
    let needle: Vec<u8> = std::iter::repeat_n(b'd', 4096).collect();
    let at = bytes
        .windows(needle.len())
        .position(|w| w == needle.as_slice())
        .expect("last block must be present verbatim in store mode");
    bytes[at] = b'X';
    let tampered = td.join("tampered.onelf");
    std::fs::write(&tampered, &bytes).unwrap();
    std::fs::set_permissions(
        &tampered,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    let mut run = Command::new(&tampered);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let out = run_package(&mut run);

    // However the runtime unpacks it, the altered bytes must never reach
    // the caller: either the read fails or extraction refuses outright.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains('X'),
        "tampered bytes must not be served: {}",
        &stdout[..stdout.len().min(200)]
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Every inspection command parses whatever file it is handed, so a footer
/// claiming regions the file cannot back must produce an error rather than
/// an allocation sized by the claim.
#[test]
fn crafted_footer_is_refused_by_every_reader() {
    let td = workdir("crafted");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), "#!/bin/sh\n");

    let good = td.join("good.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", good.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    // The footer sits in the last 76 bytes; manifest_compressed is 8 bytes
    // at offset 20 within it.
    let mut bytes = std::fs::read(&good).unwrap();
    let footer_at = bytes.len() - 76;
    bytes[footer_at + 20..footer_at + 28].copy_from_slice(&u64::MAX.to_le_bytes());
    let bad = td.join("bad.onelf");
    std::fs::write(&bad, &bytes).unwrap();

    for cmd in ["info", "list", "verify"] {
        let o = Command::new(onelf())
            .arg(cmd)
            .arg(&bad)
            .output()
            .unwrap_or_else(|e| panic!("spawn onelf {cmd}: {e}"));
        // A clean exit code, not a signal: sizing an allocation from the
        // crafted field used to abort the process instead of reporting.
        assert_eq!(
            o.status.code(),
            Some(1),
            "`onelf {cmd}` must exit with an error, not die on a bad alloc"
        );
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("out of bounds"),
            "`onelf {cmd}` must name the bounds failure, got: {err}"
        );
    }

    let o = Command::new(onelf())
        .args(["extract"])
        .arg(&bad)
        .args(["-o"])
        .arg(td.join("out"))
        .output()
        .expect("spawn onelf extract");
    assert_eq!(
        o.status.code(),
        Some(1),
        "extract must exit with an error, not die on a bad alloc"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// An AppDir usually exposes its launcher as a symlink, so an entrypoint
/// must be able to target one. Symlinks used to be left out of the path
/// index, which reported the launcher as missing from its own directory.
#[test]
fn entrypoint_may_target_a_symlink() {
    let td = workdir("symlink-ep");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/real"), "#!/bin/sh\necho ran\n");
    std::fs::set_permissions(
        app.join("bin/real"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("real", app.join("bin/launch")).unwrap();

    let out = td.join("pkg.onelf");
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", out.to_str().unwrap()])
        .args(["--command", "bin/launch", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(
        o.status.success(),
        "packing a symlink entrypoint must succeed:\n{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(out.is_file());

    // Packing is only half of it: the runtime has to resolve the symlink
    // entry to its target and execute through it.
    let mut run = Command::new(&out);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let r = run_package(&mut run);
    assert!(
        r.status.success(),
        "running through a symlink entrypoint failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "ran");

    let _ = std::fs::remove_dir_all(&td);
}

/// A default entrypoint naming nothing declared, or two entrypoints sharing
/// a name, both used to be accepted and silently reinterpreted.
#[test]
fn ambiguous_entrypoints_are_refused() {
    let td = workdir("ep-validate");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/a"), "#!/bin/sh\n");
    write(&app.join("bin/b"), "#!/bin/sh\n");

    let pack = |extra: &[&str]| {
        Command::new(onelf())
            .args(["pack", app.to_str().unwrap(), "-o"])
            .arg(td.join("out.onelf"))
            .args(["--command", "bin/a", "--mtime", "0"])
            .args(extra)
            .output()
            .expect("spawn onelf pack")
    };

    let o = pack(&["--entrypoint", "x=bin/a", "--default-entrypoint", "typo"]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "an unmatched default must fail");
    assert!(
        err.contains("typo"),
        "the message must name the value: {err}"
    );

    let o = pack(&["--entrypoint", "dup=bin/a", "--entrypoint", "dup=bin/b"]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "a duplicate name must fail");
    assert!(
        err.contains("dup"),
        "the message must name the value: {err}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A packed app that launches another packed app must not hand it a mode.
/// The runtime reads `ONELF_MODE` as a directive and never falls back when
/// one is set, so reporting the chosen mode under that same name made a
/// memfd parent abort any child that was not itself memfd-eligible.
#[test]
fn nested_packages_do_not_inherit_a_forced_mode() {
    let td = workdir("nested");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();

    let src = td.join("show.c");
    write(
        &src,
        r#"#include <stdio.h>
#include <stdlib.h>
int main(void) {
    const char *forced = getenv("ONELF_MODE");
    const char *active = getenv("ONELF_ACTIVE_MODE");
    printf("forced=[%s] active=[%s]\n", forced ? forced : "", active ? active : "");
    return 0;
}
"#,
    );
    if !cc(&src, &app.join("bin/show")) {
        return;
    }
    write(
        &app.join("onelf.toml"),
        "[package]\nname=\"nested\"\ncommand=\"bin/show\"\n",
    );

    let mut c = Command::new(onelf());
    c.arg("build").current_dir(&app);
    if let Some(pe) = patchelf() {
        c.env("ONELF_PATCHELF", pe);
    }
    let o = c.output().expect("spawn onelf build");
    assert!(
        o.status.success(),
        "build failed:\n{}",
        String::from_utf8_lossy(&o.stderr)
    );

    let pkg = app.join("nested.onelf");
    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("forced=[]"),
        "the child must not see a forced mode: {stdout}"
    );
    assert!(
        !stdout.contains("active=[]"),
        "the active mode must still be reported: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Default behaviour: the package's own bin/ is prepended to PATH
/// (re-exec-safe), and `[env]` values expand against the *live*
/// environment at runtime (so `$${HOME}` defers to runtime, and the
/// PATH prefix prepends rather than replaces).
#[test]
fn bin_on_path_by_default_and_runtime_env_expansion() {
    let td = workdir("defpath");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    let result = td.join("result");

    // Helper that exists ONLY in the package bin/ (exit 77 if reached).
    let hsrc = td.join("probe.c");
    write(&hsrc, "int main(void){return 77;}\n");
    if !cc(&hsrc, &app.join("bin/onelf_helper")) {
        return; // no compiler: documented soft-skip
    }

    let asrc = td.join("app.c");
    write(
        &asrc,
        &format!(
            r#"#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
int main(void) {{
    FILE *f = fopen("{res}", "w");
    fprintf(f, "PATH=%s\n", getenv("PATH") ? getenv("PATH") : "(null)");
    fprintf(f, "FOO=%s\n", getenv("ONELF_IT_FOO") ? getenv("ONELF_IT_FOO") : "(null)");
    int rc = 127;
    if (fork() == 0) {{
        execvp("onelf_helper", (char *[]){{ "onelf_helper", NULL }});
        _exit(127);
    }}
    int st; wait(&st); rc = WEXITSTATUS(st);
    fprintf(f, "HELPER=%d\n", rc);
    fclose(f);
    return 0;
}}"#,
            res = result.display()
        ),
    );
    if !cc(&asrc, &app.join("bin/app")) {
        return;
    }

    write(
        &app.join("onelf.toml"),
        "[package]\nname=\"defpath\"\ncommand=\"bin/app\"\n\n\
         [env]\nONELF_IT_FOO=\"pre-${ONELF_DIR}-$${HOME}-post\"\n",
    );

    let mut cmd = Command::new(onelf());
    cmd.arg("build").current_dir(&app);
    if let Some(pe) = patchelf() {
        cmd.env("ONELF_PATCHELF", pe);
    }
    let o = cmd.output().expect("spawn onelf build");
    assert!(
        o.status.success(),
        "build failed:\n{}",
        String::from_utf8_lossy(&o.stderr)
    );

    let pkg = app.join("defpath.onelf");
    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("HOME", "/xyzhome")
        .env("PATH", "/sentinel/dir");
    isolate(&mut run, &td);
    let st = run_package(&mut run).status;
    assert!(st.success());

    let r = std::fs::read_to_string(&result).expect("result file");
    let path_line = r.lines().find(|l| l.starts_with("PATH=")).unwrap_or("");
    // Default: ${ONELF_DIR}/bin prepended to the inherited PATH (not replacing it).
    assert!(
        path_line.contains("/bin:/sentinel/dir"),
        "expected bin/ prepended to inherited PATH, got: {path_line}"
    );
    // $$ deferred to runtime: HOME must be the *runtime* value, not the
    // packer's HOME at build time.
    let foo = r.lines().find(|l| l.starts_with("FOO=")).unwrap_or("");
    assert!(
        foo.starts_with("FOO=pre-/") && foo.ends_with("-/xyzhome-post"),
        "runtime env expansion wrong: {foo}"
    );
    // The bundled helper resolves via the defaulted PATH.
    assert!(
        r.contains("HELPER=77"),
        "bundled helper not found via default PATH:\n{r}"
    );

    // Run again with NO PATH at all (sandbox/clearenv shape): the
    // `${PATH:-/usr/bin:/bin}` default must fall back to system dirs,
    // with NO dangling empty element, and the helper still resolves.
    let mut run = Command::new(&pkg);
    run.env_clear().env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let st = run_package(&mut run).status;
    assert!(st.success());
    let r = std::fs::read_to_string(&result).expect("result file");
    let path_line = r.lines().find(|l| l.starts_with("PATH=")).unwrap_or("");
    assert!(
        path_line.ends_with("/bin:/usr/bin:/bin"),
        "empty PATH should fall back to /usr/bin:/bin (no dangling ':'), got: {path_line}"
    );
    assert!(
        !path_line.ends_with(':'),
        "PATH must not end in an empty element: {path_line}"
    );
    assert!(
        r.contains("HELPER=77"),
        "bundled helper not found with fallback PATH:\n{r}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A read-only (0555) directory in the source tree must not break
/// extraction of its children: directory modes are applied
/// deepest-first after files are written.
#[test]
fn readonly_directory_children_still_extract() {
    use std::os::unix::fs::PermissionsExt;

    let td = workdir("ro-dir");
    let app = td.join("app");
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");
    write(&app.join("ro/data.txt"), "payload\n");
    // Mark the directory read-only + execute (0555): its child must still
    // extract even though the dir itself forbids writes.
    std::fs::set_permissions(app.join("ro"), std::fs::Permissions::from_mode(0o555)).unwrap();

    let pkg = td.join("ro.onelf");
    let o = run_onelf(
        &[
            "pack",
            "--command",
            "bin/run.sh",
            "--output",
            pkg.to_str().unwrap(),
            app.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "pack: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let out = td.join("out");
    let o = run_onelf(
        &[
            "extract",
            pkg.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "extract: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let got = std::fs::read_to_string(out.join("ro/data.txt")).expect("child under 0555 dir");
    assert_eq!(got, "payload\n");
    // The recorded directory mode is still applied.
    let mode = std::fs::metadata(out.join("ro"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o555,
        "directory mode must be applied after extraction"
    );

    // Re-extract into the SAME output dir: `out/ro` now pre-exists as 0555,
    // and extraction must still rewrite its children instead of failing
    // with EACCES.
    let o = run_onelf(
        &[
            "extract",
            pkg.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "re-extract into a pre-existing read-only dir: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let got = std::fs::read_to_string(out.join("ro/data.txt")).expect("child re-extracted");
    assert_eq!(got, "payload\n");

    // Restore writable perms on both the source and output read-only dirs so
    // remove_dir_all can delete their children; otherwise `td` leaks.
    for ro in [app.join("ro"), out.join("ro")] {
        let _ = std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755));
    }
    let _ = std::fs::remove_dir_all(&td);
}

/// The `.onelf/` namespace is reserved for injected metadata, so a source
/// tree that already contains such a path is rejected rather than silently
/// producing a duplicate entry.
#[test]
fn source_onelf_path_is_rejected() {
    let td = workdir("reserved");
    let app = td.join("app");
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");
    write(&app.join(".onelf/env"), "FOO=bar\n");

    let pkg = td.join("r.onelf");
    let o = run_onelf(
        &[
            "pack",
            "--command",
            "bin/run.sh",
            "--output",
            pkg.to_str().unwrap(),
            app.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        !o.status.success(),
        "packing a reserved .onelf/ path must fail"
    );
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains(".onelf"),
        "error should name the reserved path; got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Recipe `${VAR}` expansion runs after the TOML is parsed, so an env value
/// containing a quote and newline cannot inject a new recipe key. The
/// variable is set on the child process only, never on this test process.
#[test]
fn recipe_expansion_cannot_inject_keys() {
    let td = workdir("recipe-inj");
    let app = td.join("app");
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");
    write(
        &app.join("onelf.toml"),
        "[package]\ncommand = \"bin/run.sh\"\ndescription = \"${ONELF_INJ}\"\n\n[bundle]\nskip = true\n",
    );

    let pkg = td.join("i.onelf");
    // Spliced into raw TOML before parsing, this would close the string and
    // open a `name` key. Post-parse expansion keeps it as one value.
    let payload = "evil\"\nname = \"HACKED";
    let o = Command::new(onelf())
        .args([
            "build",
            app.to_str().unwrap(),
            "--output",
            pkg.to_str().unwrap(),
        ])
        .env("ONELF_INJ", payload)
        .output()
        .expect("spawn onelf build");
    assert!(
        o.status.success(),
        "build: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let o = run_onelf(&["info", pkg.to_str().unwrap()], None);
    let info = String::from_utf8_lossy(&o.stdout);
    // If expansion happened before parse, the value would have split into a
    // separate `name` key and the description would be just "evil"; the
    // marker only survives inside the description when expansion is
    // post-parse.
    assert!(
        info.contains("HACKED"),
        "injected marker must survive as a literal description value:\n{info}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// The documented user asset dirs `.onelf/icons/` and `.onelf/desktop/`
/// are accepted from source and round-trip through `icon`/`desktop`,
/// while the rest of the `.onelf/` namespace stays reserved.
#[test]
fn source_onelf_assets_are_accepted() {
    let td = workdir("assets");
    let app = td.join("app");
    write(&app.join("bin/run.sh"), "#!/bin/sh\necho hi\n");
    write(&app.join(".onelf/icons/default.png"), "PNGDATA");
    write(
        &app.join(".onelf/desktop/default.desktop"),
        "[Desktop Entry]\nName=App\nExec=run.sh\n",
    );

    let pkg = td.join("a.onelf");
    let o = run_onelf(
        &[
            "pack",
            "--command",
            "bin/run.sh",
            "--output",
            pkg.to_str().unwrap(),
            app.to_str().unwrap(),
        ],
        None,
    );
    assert!(
        o.status.success(),
        "packing .onelf/icons and .onelf/desktop must succeed; got: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let icon_out = td.join("out.png");
    let i = run_onelf(
        &[
            "icon",
            pkg.to_str().unwrap(),
            "-o",
            icon_out.to_str().unwrap(),
        ],
        None,
    );
    assert!(i.status.success(), "icon extract failed");
    assert_eq!(std::fs::read(&icon_out).unwrap(), b"PNGDATA");

    let desk_out = td.join("out.desktop");
    let d = run_onelf(
        &[
            "desktop",
            pkg.to_str().unwrap(),
            "-o",
            desk_out.to_str().unwrap(),
        ],
        None,
    );
    assert!(d.status.success(), "desktop extract failed");
    assert!(
        String::from_utf8_lossy(&std::fs::read(&desk_out).unwrap()).contains("Name=App"),
        "extracted desktop file should carry the source content"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A statically linked entrypoint is what memfd mode is for, and forcing the
/// mode has to keep working for it. This is the control for the two refusals
/// below: they must reject the impossible case without taking this with them.
#[test]
fn a_static_entrypoint_still_runs_from_a_memfd() {
    let td = workdir("memfd-static");
    let app = td.join("app");
    let src = td.join("hello.c");
    write(
        &src,
        "#include <stdio.h>\nint main(){puts(\"from-memfd\");return 0;}\n",
    );

    let bin = app.join("bin/hello");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    let compiler = if have("cc") {
        "cc"
    } else if have("gcc") {
        "gcc"
    } else {
        eprintln!("skip: no C compiler available");
        return;
    };
    let built = Command::new(compiler)
        .args(["-O0", "-static", "-o"])
        .arg(&bin)
        .arg(&src)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !built {
        eprintln!("skip: no static libc available to link against");
        let _ = std::fs::remove_dir_all(&td);
        return;
    }

    let pkg = td.join("hello.onelf");
    let out = run_onelf(
        &[
            "pack",
            app.to_str().unwrap(),
            "-o",
            pkg.to_str().unwrap(),
            "--command",
            "bin/hello",
        ],
        None,
    );
    assert!(out.status.success(), "pack failed: {out:?}");

    let mut c = Command::new(&pkg);
    c.env("ONELF_MODE", "memfd");
    isolate(&mut c, &td);
    let run = run_package(&mut c);
    assert!(
        run.status.success(),
        "forced memfd must still run a static entrypoint: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("from-memfd"),
        "expected the program's own output"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Forcing memfd on an entrypoint that needs bundled libraries has to fail
/// with a message about the mode. Left alone it execs successfully and the
/// *loader* fails afterwards, naming a library, in a process onelf no longer
/// controls, so the mode that caused it is never mentioned.
#[test]
fn forcing_memfd_on_a_linked_entrypoint_says_so() {
    // No "memfd" in the directory name: the failure this guards against
    // prints the package's own path, so a tag containing "memfd" would
    // satisfy the assertion below whether or not the guard exists.
    let td = workdir("forced-mode");
    let app = td.join("app");
    let src = td.join("m.c");
    let bin = app.join("bin/m");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    if !cc_libm(&src, &bin) {
        eprintln!("skip: no C compiler available");
        let _ = std::fs::remove_dir_all(&td);
        return;
    }

    let pkg = td.join("m.onelf");
    let out = run_onelf(
        &[
            "pack",
            app.to_str().unwrap(),
            "-o",
            pkg.to_str().unwrap(),
            "--command",
            "bin/m",
        ],
        None,
    );
    assert!(out.status.success(), "pack failed: {out:?}");

    let mut c = Command::new(&pkg);
    c.env("ONELF_MODE", "memfd");
    isolate(&mut c, &td);
    let run = run_package(&mut c);
    assert!(!run.status.success(), "forced memfd must not succeed here");
    let err = String::from_utf8_lossy(&run.stderr);
    assert!(
        err.contains("ONELF_MODE") && err.contains("memfd mode cannot satisfy"),
        "the error has to point at the mode, not at whichever library the \
         loader happened to miss, got: {err}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// `--memfd` on a bundle-libs entrypoint builds a package that fails for
/// every user at every launch, because bundle-libs links it against a library
/// it puts inside the package. Refuse at pack time and write nothing.
#[test]
fn packing_refuses_memfd_for_a_bundled_entrypoint() {
    let Some(_pe) = patchelf() else {
        eprintln!("skip: patchelf not available");
        return;
    };
    let td = workdir("memfd-pack");
    let app = td.join("app");
    let src = td.join("m.c");
    let bin = app.join("bin/m");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    if !cc_libm(&src, &bin) {
        eprintln!("skip: no C compiler available");
        let _ = std::fs::remove_dir_all(&td);
        return;
    }

    let bundled = run_onelf(&["bundle-libs", app.to_str().unwrap()], None);
    assert!(bundled.status.success(), "bundle-libs failed: {bundled:?}");

    let pkg = td.join("m.onelf");
    let out = run_onelf(
        &[
            "pack",
            app.to_str().unwrap(),
            "-o",
            pkg.to_str().unwrap(),
            "--command",
            "bin/m",
            "--memfd",
        ],
        None,
    );
    assert!(!out.status.success(), "pack must refuse --memfd here");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--memfd"),
        "the error has to name the flag, got: {err}"
    );
    assert!(
        !pkg.exists(),
        "a package that cannot run must not be left behind"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A process that daemonizes must keep working after the launcher returns.
///
/// It forks a background copy, lets the foreground exit, and closes every
/// descriptor it inherited. That last part hangs up the death pipe the FUSE
/// mode waits on, so the signal says "finished" while the real work is only
/// starting. Tearing the filesystem down there leaves the surviving process
/// blocked on its next read, which looks exactly like the app having hung.
#[test]
fn a_daemonized_process_outlives_the_launcher() {
    if !fuse_available() {
        eprintln!("skip: FUSE is not mountable here");
        return;
    }
    let td = workdir("daemonize");
    let app = td.join("app");
    let src = td.join("d.c");
    let bin = app.join("bin/d");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();

    let result = td.join("result");
    write(&app.join("data.txt"), "payload\n");
    write(
        &src,
        &format!(
            "#include <unistd.h>\n#include <stdio.h>\n#include <stdlib.h>\n\
             int main(void){{\n\
             \x20 if (fork() != 0) {{ _exit(0); }}\n\
             \x20 setsid();\n\
             \x20 for (int fd = 3; fd < 256; fd++) close(fd);\n\
             \x20 sleep(2);\n\
             \x20 const char *d = getenv(\"ONELF_DIR\");\n\
             \x20 char p[512];\n\
             \x20 snprintf(p, sizeof p, \"%s/data.txt\", d ? d : \"/nonexistent\");\n\
             \x20 FILE *in = fopen(p, \"r\");\n\
             \x20 FILE *out = fopen(\"{}\", \"w\");\n\
             \x20 if (out) {{ fprintf(out, in ? \"READ-OK\" : \"READ-FAILED\"); fclose(out); }}\n\
             \x20 return 0;\n}}\n",
            result.display()
        ),
    );
    if !cc(&src, &bin) {
        let _ = std::fs::remove_dir_all(&td);
        return;
    }

    let pkg = td.join("d.onelf");
    let out = run_onelf(
        &[
            "pack",
            app.to_str().unwrap(),
            "-o",
            pkg.to_str().unwrap(),
            "--command",
            "bin/d",
        ],
        None,
    );
    assert!(out.status.success(), "pack failed: {out:?}");

    let mut c = Command::new(&pkg);
    c.env("ONELF_MODE", "fuse");
    isolate(&mut c, &td);
    let run = run_package(&mut c);
    assert!(
        run.status.success(),
        "launcher must return promptly and successfully: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    // The daemonized process reads the mount two seconds after the launcher
    // is gone, so this waits past that before deciding.
    let mut got = String::new();
    for _ in 0..60 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if let Ok(s) = std::fs::read_to_string(&result) {
            got = s;
            break;
        }
    }
    assert_eq!(
        got, "READ-OK",
        "the daemonized process could not read the package after the launcher exited"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// The launcher has to return when the process it started exits, not when the
/// last thing that process spawned exits.
///
/// A caller that starts a daemon does the same thing every time: wait for the
/// launcher, then look for whatever the daemon was supposed to set up. If the
/// launcher instead waits for the daemon to finish, that check runs after the
/// daemon has already gone, and a daemon that started perfectly well is
/// reported dead.
#[test]
fn the_launcher_returns_without_waiting_for_a_daemon() {
    if !fuse_available() {
        eprintln!("skip: FUSE is not mountable here");
        return;
    }
    let td = workdir("prompt-return");
    let app = td.join("app");
    let src = td.join("p.c");
    let bin = app.join("bin/p");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();

    // Forks a background process that outlives the launcher by 5 seconds,
    // the same shape as a daemon holding a socket open.
    // Detaches from every inherited descriptor first, exactly as a daemon
    // does. Holding the launcher's stdout would make the test measure the
    // harness waiting on a pipe rather than the launcher waiting on a process.
    write(
        &src,
        "#include <unistd.h>\n#include <fcntl.h>\nint main(void){\n\
         \x20 if (fork() != 0) { _exit(0); }\n\
         \x20 setsid();\n\
         \x20 int n = open(\"/dev/null\", O_RDWR);\n\
         \x20 dup2(n, 0); dup2(n, 1); dup2(n, 2);\n\
         \x20 for (int fd = 3; fd < 256; fd++) close(fd);\n\
         \x20 sleep(5);\n\
         \x20 return 0;\n}\n",
    );
    if !cc(&src, &bin) {
        let _ = std::fs::remove_dir_all(&td);
        return;
    }

    let pkg = td.join("p.onelf");
    let out = run_onelf(
        &[
            "pack",
            app.to_str().unwrap(),
            "-o",
            pkg.to_str().unwrap(),
            "--command",
            "bin/p",
        ],
        None,
    );
    assert!(out.status.success(), "pack failed: {out:?}");

    let mut c = Command::new(&pkg);
    c.env("ONELF_MODE", "fuse");
    isolate(&mut c, &td);
    let started = std::time::Instant::now();
    let run = run_package(&mut c);
    let waited = started.elapsed();

    assert!(run.status.success(), "launcher failed: {run:?}");
    assert!(
        waited < std::time::Duration::from_secs(3),
        "launcher waited {waited:?} for a background process that sleeps 5s; \
         it should return as soon as the process it started exits"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// Pack a package whose entrypoint is the shell script `body`.
fn pack_script(td: &Path, tag: &str, body: &str) -> PathBuf {
    let app = td.join(tag);
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(&app.join("bin/run"), body);
    std::fs::set_permissions(
        app.join("bin/run"),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();
    let pkg = td.join(format!("{tag}.onelf"));
    let o = Command::new(onelf())
        .args(["pack", app.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0", "--name", tag])
        .output()
        .expect("spawn onelf pack");
    assert!(
        o.status.success(),
        "pack failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    pkg
}

/// The mountpoint under `run_dir` that this process's mount namespace
/// currently lists, if any.
fn helper_mount_under(run_dir: &Path) -> Option<PathBuf> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let prefix = format!("{}/onelf-", run_dir.display());
    info.lines().find_map(|line| {
        let mp = line.split(' ').nth(4)?.replace("\\040", " ");
        mp.starts_with(&prefix).then(|| PathBuf::from(mp))
    })
}

/// Launch `pkg` through the host's FUSE helper and wait for its mount to
/// appear. `None` when the helper cannot mount here, which is a documented
/// soft-skip like the other environment probes.
fn launch_via_helper(pkg: &Path, td: &Path) -> Option<(std::process::Child, PathBuf)> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(pkg);
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("ONELF_MODE", "fuse")
        .env("ONELF_FUSE_NO_NAMESPACE", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    isolate(&mut cmd, td);
    let mut child = cmd.spawn().expect("spawn package");
    let run_dir = td.join("xdg-run");
    for _ in 0..200 {
        if let Some(mp) = helper_mount_under(&run_dir) {
            return Some((child, mp));
        }
        if child.try_wait().unwrap().is_some() {
            eprintln!("skip: the FUSE helper cannot mount here");
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let _ = child.kill();
    eprintln!("skip: the FUSE helper mount did not appear");
    None
}

/// A mount served through the host's FUSE helper outlives a runtime that
/// was killed, and the kernel cannot tear it down on its own. The next
/// launch of any package reclaims it.
#[test]
fn a_dead_helper_mount_is_reclaimed_by_the_next_launch() {
    if !fuse_available() || !have("fusermount3") {
        return; // documented soft-skip
    }
    let td = workdir("deadmount");
    let sleeper = pack_script(&td, "sleeper", "#!/bin/sh\nsleep 4\n");
    let other = pack_script(&td, "other", "#!/bin/sh\necho OTHER\n");

    let Some((mut child, mountpoint)) = launch_via_helper(&sleeper, &td) else {
        return;
    };
    // The runtime and the app it launched share the mountpoint lock, so
    // the whole group has to die for the mount to count as abandoned.
    kill_group(&child);
    child.wait().unwrap();
    assert!(
        helper_mount_under(&td.join("xdg-run")).is_some(),
        "the mount outlives its killed runtime"
    );

    let mut run = Command::new(&other);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    assert!(
        reclaimed_by(&mut run, &td),
        "the dead mount is gone from the namespace"
    );
    assert!(
        std::fs::symlink_metadata(&mountpoint).is_err(),
        "the dead mount's directory is removed"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A live helper mount belongs to a running instance and is left alone by
/// another package's launch.
#[test]
fn a_live_helper_mount_survives_another_launch() {
    if !fuse_available() || !have("fusermount3") {
        return; // documented soft-skip
    }
    let td = workdir("livemount");
    let sleeper = pack_script(&td, "sleeper", "#!/bin/sh\nsleep 4\n");
    let other = pack_script(&td, "other", "#!/bin/sh\necho OTHER\n");

    let Some((mut child, mountpoint)) = launch_via_helper(&sleeper, &td) else {
        return;
    };

    let mut run = Command::new(&other);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    assert!(out.status.success());

    assert_eq!(
        helper_mount_under(&td.join("xdg-run")),
        Some(mountpoint.clone()),
        "the live mount is still in the namespace"
    );
    assert!(
        mountpoint.join("bin/run").is_file(),
        "the live mount still answers"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "the instance is still running"
    );

    // Once the instance is gone its mount is abandoned, and the next launch
    // reclaims it, which also leaves nothing behind on this machine.
    kill_group(&child);
    child.wait().unwrap();
    assert!(
        reclaimed_by(&mut run, &td),
        "the abandoned mount is reclaimed"
    );
    let _ = std::fs::remove_dir_all(&td);
}

/// Launch `run` until no helper mount is left under the test's runtime
/// directory. A killed group's processes release the mountpoint lock as
/// they die, which is not instantaneous, and a launch that arrives before
/// the last of them is right to leave the mount alone.
fn reclaimed_by(run: &mut Command, td: &Path) -> bool {
    for _ in 0..40 {
        let out = run_package(run);
        assert!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("OTHER"),
            "the next package runs: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        if helper_mount_under(&td.join("xdg-run")).is_none() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// SIGKILL `child` and every process in its group.
fn kill_group(child: &std::process::Child) {
    let st = Command::new("kill")
        .args(["-9", "--", &format!("-{}", child.id())])
        .status()
        .expect("spawn kill");
    assert!(st.success());
}

/// The runtime directory mode runs the package from the private per-user
/// directory and leaves nothing behind when it exits.
#[test]
fn rundir_mode_runs_and_leaves_nothing_behind() {
    let td = workdir("rundir");
    let pkg = pack_script(
        &td,
        "app",
        "#!/bin/sh\necho mode=$ONELF_ACTIVE_MODE\necho dir=$ONELF_DIR\n",
    );

    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("ONELF_MODE", "rundir");
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("mode=rundir"),
        "rundir mode runs:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dir = stdout
        .lines()
        .find_map(|l| l.strip_prefix("dir="))
        .expect("the app reports its directory");
    assert!(
        Path::new(dir).starts_with(td.join("xdg-run")),
        "the tree lives in the private runtime directory: {dir}"
    );
    assert!(
        std::fs::symlink_metadata(dir).is_err(),
        "the tree is removed after exit"
    );
    assert_eq!(
        std::fs::read_dir(td.join("xdg-cache")).unwrap().count(),
        0,
        "the persistent cache is untouched"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A rundir tree whose every process was killed is removed by the next
/// launch of any package.
#[test]
fn an_abandoned_rundir_tree_is_reclaimed_by_the_next_launch() {
    use std::os::unix::process::CommandExt;

    let td = workdir("rundirkill");
    let sleeper = pack_script(&td, "sleeper", "#!/bin/sh\nsleep 4\n");
    let other = pack_script(&td, "other", "#!/bin/sh\necho OTHER\n");
    let run_dir = td.join("xdg-run");

    let mut cmd = Command::new(&sleeper);
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("ONELF_MODE", "rundir")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    isolate(&mut cmd, &td);
    let mut child = cmd.spawn().expect("spawn package");

    let tree_of = |prefix: &str| {
        std::fs::read_dir(&run_dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
                    && p.join("bin/run").is_file()
            })
    };
    let mut tree = None;
    for _ in 0..200 {
        tree = tree_of("onelf-sleepe");
        if tree.is_some() {
            break;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the sleeper exited early"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let tree = tree.expect("the tree appears");

    kill_group(&child);
    child.wait().unwrap();
    assert!(tree.is_dir(), "the tree outlives its killed processes");

    let mut run = Command::new(&other);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("ONELF_MODE", "rundir");
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    assert!(out.status.success());
    assert!(
        std::fs::symlink_metadata(&tree).is_err(),
        "the abandoned tree is removed"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A block whose bytes no longer match the manifest is refused before
/// anything runs, and the file it belonged to is named.
#[test]
fn rundir_refuses_a_tampered_block() {
    let td = workdir("rundirtamper");
    let pkg = pack_script(&td, "app", "#!/bin/sh\necho SHOULD NOT RUN\n");

    let mut data = std::fs::read(&pkg).unwrap();
    let footer_at = data.len() - 76;
    let payload_offset =
        u64::from_le_bytes(data[footer_at + 36..footer_at + 44].try_into().unwrap()) as usize;
    let payload_size =
        u64::from_le_bytes(data[footer_at + 44..footer_at + 52].try_into().unwrap()) as usize;
    data[payload_offset + payload_size / 2] ^= 0xff;
    let tampered = td.join("tampered.onelf");
    std::fs::write(&tampered, data).unwrap();
    std::fs::set_permissions(
        &tampered,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    let mut run = Command::new(&tampered);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("ONELF_MODE", "rundir");
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("SHOULD NOT RUN"),
        "nothing runs from a tampered package"
    );
    // Which entry the flipped byte lands in depends on the layout; the
    // refusal names whichever it was.
    assert!(
        stderr.contains("extract failed: ")
            && (stderr.contains("bin/run") || stderr.contains(".onelf/")),
        "the refusal names the file:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// The persistent cache is the one mode that leaves something behind, so
/// it runs only when the publisher, the user, or a forced mode asks.
#[test]
fn the_cache_is_used_only_on_request() {
    let td = workdir("cacheoptin");
    let pkg = pack_script(&td, "app", "#!/bin/sh\necho mode=$ONELF_ACTIVE_MODE\n");
    let cache_root = td.join("xdg-cache");
    let cache_entries = || std::fs::read_dir(&cache_root).unwrap().count();

    // With every other mode refusing, and nothing asking for the cache,
    // the launch fails and the cache root stays empty. The private
    // directory is owned by this user and closed to others, so it is
    // accepted, but nothing can be created inside it.
    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    run.env("XDG_RUNTIME_DIR", "/proc/self/fd");
    let out = run_package(&mut run);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "no mode is available:\n{stderr}");
    assert!(
        stderr.contains("ONELF_CACHE"),
        "the failure says how to allow the cache:\n{stderr}"
    );
    assert_eq!(cache_entries(), 0, "an unrequested cache is never written");

    // The user asks for it.
    run.env("ONELF_CACHE", "1");
    let out = run_package(&mut run);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("mode=cache"),
        "the requested cache runs the package:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cache_entries() > 0);

    // The publisher asks for it.
    let requested = td.join("requested.onelf");
    let o = Command::new(onelf())
        .args(["pack", td.join("app").to_str().unwrap()])
        .args(["-o", requested.to_str().unwrap()])
        .args(["--command", "bin/run", "--mtime", "0", "--cache"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());
    let mut run = Command::new(&requested);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap());
    isolate(&mut run, &td);
    run.env("XDG_RUNTIME_DIR", "/proc/self/fd");
    let out = run_package(&mut run);
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("mode=cache"),
        "a package packed with the cache requested falls back to it: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A forced cache mode still bypasses everything else.
    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("ONELF_MODE", "cache");
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("mode=cache"));

    let _ = std::fs::remove_dir_all(&td);
}

/// `onelf run` makes the same host-library decision as the packed runtime:
/// a host driver reaches its newer dependency through the link farm, and no
/// host directory is placed on the library path.
#[test]
fn onelf_run_resolves_host_libraries_like_the_runtime() {
    let td = workdir("runresolver");
    let app = td.join("app");
    let host = td.join("host");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    std::fs::create_dir_all(app.join("lib")).unwrap();
    std::fs::create_dir_all(&host).unwrap();

    let fixture_c = td.join("fixture.c");
    write(
        &fixture_c,
        "int fix_value(void){return 1;}\nint fix_extra(void){return 2;}\n",
    );
    let old_map = td.join("old.map");
    write(&old_map, "FIX_1.0 { global: fix_value; local: *; };\n");
    let new_map = td.join("new.map");
    write(
        &new_map,
        "FIX_1.0 { global: fix_value; local: *; };\nFIX_2.0 { global: fix_extra; } FIX_1.0;\n",
    );
    if !cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libfixture.so.1",
            &format!("-Wl,--version-script={}", old_map.display()),
            fixture_c.to_str().unwrap(),
        ],
        &app.join("lib/libfixture.so.1"),
    ) {
        return; // no compiler: documented soft-skip
    }
    let host_fixture = host.join("libfixture.so.1");
    assert!(cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libfixture.so.1",
            &format!("-Wl,--version-script={}", new_map.display()),
            fixture_c.to_str().unwrap(),
        ],
        &host_fixture,
    ));
    let gl_c = td.join("gl.c");
    write(
        &gl_c,
        "int fix_extra(void);\nint gl_probe(void){return fix_extra();}\n",
    );
    let host_gl = host.join("libGL.so.1");
    assert!(cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libGL.so.1",
            gl_c.to_str().unwrap(),
            &format!("-L{}", host.display()),
            "-l:libfixture.so.1",
        ],
        &host_gl,
    ));
    let app_c = td.join("app.c");
    write(
        &app_c,
        r#"#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
int main(void) {
    const char *lp = getenv("LD_LIBRARY_PATH");
    printf("LD_LIBRARY_PATH=%s\n", lp ? lp : "");
    void *h = dlopen("libGL.so.1", RTLD_NOW);
    if (!h) { printf("dlopen: %s\n", dlerror()); return 1; }
    int (*probe)(void) = dlsym(h, "gl_probe");
    if (!probe) { printf("dlsym: %s\n", dlerror()); return 1; }
    printf("probe=%d\n", probe());
    return 0;
}
"#,
    );
    assert!(cc_with(
        &[app_c.to_str().unwrap(), "-ldl"],
        &app.join("bin/app")
    ));

    let cache = td.join("ld.so.cache");
    std::fs::write(&cache, ld_so_cache(&[&host_gl, &host_fixture])).unwrap();

    let launch = |no_resolver: bool| {
        let mut run = Command::new(onelf());
        run.args(["run", app.to_str().unwrap(), "--command", "bin/app"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", td.to_str().unwrap())
            .env("ONELF_LD_CACHE", &cache);
        if no_resolver {
            run.env("ONELF_NO_RESOLVER", "1");
        }
        isolate(&mut run, &td);
        run.output().expect("spawn onelf run")
    };

    let without = launch(true);
    assert!(
        !without.status.success(),
        "without the resolver the driver must fail to bind: {}",
        String::from_utf8_lossy(&without.stdout)
    );

    let with = launch(false);
    let stdout = String::from_utf8_lossy(&with.stdout);
    assert!(
        with.status.success() && stdout.contains("probe=2"),
        "with the resolver the host driver binds against the host copy:\n{stdout}{}",
        String::from_utf8_lossy(&with.stderr)
    );
    let lp = stdout
        .lines()
        .find_map(|l| l.strip_prefix("LD_LIBRARY_PATH="))
        .expect("the app reports its library path");
    for dir in lp.split(':').filter(|d| !d.is_empty()) {
        assert!(
            Path::new(dir).starts_with(&td),
            "a host directory reached the library path: {dir}"
        );
    }

    let _ = std::fs::remove_dir_all(&td);
}

/// A synthetic Arch-style sysroot: a rootfs with a pacman database and
/// compiled fixtures, archived with the host's `tar`.
struct SysrootFixture {
    rootfs: PathBuf,
    archive: PathBuf,
    platform_line: PathBuf,
    policy: PathBuf,
    /// A "host" holding the platform-line driver, for running the result.
    ld_cache: PathBuf,
    /// The GL build the sysroot pins, made with `onelf sysroot pack-gl`.
    gl_build: PathBuf,
    gl_hash: String,
}

/// Pack a GL build holding `libGL.so.1` from the rootfs and return its
/// path and hash, as `pack-gl` prints it.
fn pack_gl_build(td: &Path, rootfs: &Path) -> (PathBuf, String) {
    let tree = td.join("gl-tree");
    std::fs::create_dir_all(tree.join("lib")).unwrap();
    std::fs::copy(
        rootfs.join("usr/lib/libGL.so.1"),
        tree.join("lib/libGL.so.1"),
    )
    .unwrap();
    let build = td.join("gl.onelf");
    let out = Command::new(onelf())
        .args(["sysroot", "pack-gl"])
        .arg(&tree)
        .arg("-o")
        .arg(&build)
        .output()
        .expect("spawn onelf sysroot pack-gl");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hash = stdout
        .lines()
        .find_map(|l| l.strip_prefix("blake3 = \""))
        .and_then(|l| l.strip_suffix('"'))
        .unwrap_or_else(|| panic!("no hash line in:\n{stdout}"))
        .to_string();
    assert_eq!(hash.len(), 64);
    (build, hash)
}

/// Write a package's `desc` and `files` into the database under `rootfs`.
fn write_pacman_entry(
    rootfs: &Path,
    name: &str,
    version: &str,
    depends: &[&str],
    optdepends: &[&str],
    files: &[&str],
) {
    let dir = rootfs
        .join("var/lib/pacman/local")
        .join(format!("{name}-{version}"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut desc = format!("%NAME%\n{name}\n\n%VERSION%\n{version}\n\n");
    if !depends.is_empty() {
        desc.push_str(&format!("%DEPENDS%\n{}\n\n", depends.join("\n")));
    }
    if !optdepends.is_empty() {
        desc.push_str(&format!("%OPTDEPENDS%\n{}\n\n", optdepends.join("\n")));
    }
    std::fs::write(dir.join("desc"), desc).unwrap();
    let mut listing = String::from("%FILES%\n");
    for f in files {
        let mut prefix = String::new();
        for part in f.split('/').take(f.split('/').count() - 1) {
            prefix.push_str(part);
            prefix.push('/');
            listing.push_str(&prefix);
            listing.push('\n');
        }
        listing.push_str(f);
        listing.push('\n');
    }
    std::fs::write(dir.join("files"), listing).unwrap();
}

fn synthetic_sysroot(td: &Path) -> Option<SysrootFixture> {
    let rootfs = td.join("rootfs");
    for d in [
        "usr/bin",
        "usr/lib/fixture/plugins",
        "usr/lib/extra",
        "usr/share/doc/libfixture",
        "usr/share/fixture",
    ] {
        std::fs::create_dir_all(rootfs.join(d)).unwrap();
    }
    let src = td.join("src");
    std::fs::create_dir_all(&src).unwrap();

    write(&src.join("fixture.c"), "int fix_value(void){return 41;}\n");
    if !cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libfixture.so.1",
            src.join("fixture.c").to_str().unwrap(),
        ],
        &rootfs.join("usr/lib/libfixture.so.1"),
    ) {
        return None; // no compiler: documented soft-skip
    }
    for (name, out) in [
        ("plugin_a", "usr/lib/fixture/plugins/a.so"),
        ("plugin_b", "usr/lib/fixture/plugins/b.so"),
        ("extra", "usr/lib/extra/libextra.so"),
    ] {
        write(
            &src.join(format!("{name}.c")),
            &format!("int {name}(void){{return 1;}}\n"),
        );
        assert!(cc_with(
            &[
                "-shared",
                "-fPIC",
                src.join(format!("{name}.c")).to_str().unwrap()
            ],
            &rootfs.join(out),
        ));
    }
    write(&src.join("gl.c"), "int gl_probe(void){return 1;}\n");
    assert!(cc_with(
        &[
            "-shared",
            "-fPIC",
            "-Wl,-soname,libGL.so.1",
            src.join("gl.c").to_str().unwrap(),
        ],
        &rootfs.join("usr/lib/libGL.so.1"),
    ));
    write(
        &src.join("app.c"),
        "#include <stdio.h>\nint fix_value(void);\nint gl_probe(void);\nint main(void){printf(\"value=%d\\n\", fix_value() + gl_probe());return 0;}\n",
    );
    assert!(cc_with(
        &[
            src.join("app.c").to_str().unwrap(),
            &format!("-L{}", rootfs.join("usr/lib").display()),
            "-l:libfixture.so.1",
            "-l:libGL.so.1",
        ],
        &rootfs.join("usr/bin/app"),
    ));
    write(&rootfs.join("usr/share/doc/libfixture/README"), "docs\n");
    write(&rootfs.join("usr/share/fixture/data.txt"), "data\n");

    let (gl_build, gl_hash) = pack_gl_build(td, &rootfs);
    std::fs::create_dir_all(rootfs.join("etc/onelf")).unwrap();
    write(
        &rootfs.join("etc/onelf/platform.toml"),
        &format!(
            "label = \"platform-test\"\n\n[gl]\nurl = \"file://{}\"\nblake3 = \"{gl_hash}\"\n",
            gl_build.display()
        ),
    );

    // The sysroot's glibc is this machine's, copied in under the package
    // name, so the closure has a libc to bundle.
    let host_libc = ldd_path(&rootfs.join("usr/bin/app"), "libc.so.6")?;
    let host_ld = ldd_path(&rootfs.join("usr/bin/app"), "ld-linux")?;
    std::fs::copy(&host_libc, rootfs.join("usr/lib/libc.so.6")).unwrap();
    std::fs::copy(&host_ld, rootfs.join("usr/lib/ld-linux-x86-64.so.2")).unwrap();

    write_pacman_entry(
        &rootfs,
        "glibc",
        "2.99-1",
        &[],
        &[],
        &["usr/lib/libc.so.6", "usr/lib/ld-linux-x86-64.so.2"],
    );
    write_pacman_entry(
        &rootfs,
        "libfixture",
        "1.0-1",
        &["glibc>=2.30"],
        &[],
        &[
            "usr/lib/libfixture.so.1",
            "usr/lib/fixture/plugins/a.so",
            "usr/lib/fixture/plugins/b.so",
            "usr/share/doc/libfixture/README",
            "usr/share/fixture/data.txt",
        ],
    );
    write_pacman_entry(
        &rootfs,
        "extra",
        "1.0-1",
        &[],
        &[],
        &["usr/lib/extra/libextra.so"],
    );
    write_pacman_entry(
        &rootfs,
        "mesa-fake",
        "1.0-1",
        &["glibc"],
        &[],
        &["usr/lib/libGL.so.1"],
    );
    write_pacman_entry(
        &rootfs,
        "app",
        "1.0-1",
        &["libfixture", "mesa-fake", "glibc>=2.30"],
        &["extra: more features"],
        &["usr/bin/app"],
    );

    let archive = td.join("root.tar");
    let st = Command::new("tar")
        .args(["--create", "--file"])
        .arg(&archive)
        .args(["--sort=name", "--owner=0", "--group=0", "-C"])
        .arg(&rootfs)
        .arg(".")
        .status()
        .expect("spawn tar");
    assert!(st.success());

    let platform_line = td.join("platform-line.txt");
    write(&platform_line, "# the host's GL stack\nlibGL.so\n");
    let policy = td.join("policy.txt");
    write(&policy, "usr/share/doc/**\n");

    let host = td.join("host");
    std::fs::create_dir_all(&host).unwrap();
    std::fs::copy(rootfs.join("usr/lib/libGL.so.1"), host.join("libGL.so.1")).unwrap();
    let ld_cache = td.join("ld.so.cache");
    std::fs::write(&ld_cache, ld_so_cache(&[&host.join("libGL.so.1")])).unwrap();

    Some(SysrootFixture {
        rootfs,
        archive,
        platform_line,
        policy,
        ld_cache,
        gl_build,
        gl_hash,
    })
}

fn sysroot_recipe(dir: &Path, fixture: &SysrootFixture, extra: &str) {
    write(
        &dir.join("onelf.toml"),
        &format!(
            "[package]\ncommand = \"bin/app\"\nmtime = 0\n\n[sysroot]\npath = \"../sysroot\"\narchive = \"{}\"\nplatform-line = \"{}\"\npolicy = \"{}\"\n{extra}",
            fixture.archive.display(),
            fixture.platform_line.display(),
            fixture.policy.display(),
        ),
    );
}

fn onelf_build(dir: &Path) -> std::process::Output {
    Command::new(onelf())
        .arg("build")
        .current_dir(dir)
        .output()
        .expect("spawn onelf build")
}

/// Every file under the AppDir's `bin/` and `lib/` has a counterpart in
/// the sysroot, so nothing came from the packer's machine.
fn assert_bundle_is_from_sysroot(appdir: &Path, rootfs: &Path) {
    for entry in jwalk::WalkDir::new(appdir).sort(true) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(appdir).unwrap().to_path_buf();
        let rel_s = rel.to_string_lossy();
        if !(rel_s.starts_with("bin/") || rel_s.starts_with("lib/")) {
            continue;
        }
        if rel_s.contains("libonelf-env") {
            continue; // written by the packer itself
        }
        assert!(
            rootfs.join("usr").join(&rel).exists(),
            "{} has no counterpart in the sysroot",
            rel.display()
        );
    }
}

/// The closure of the entrypoint's package comes out of the sysroot with
/// its plugins and data, pruned by the platform line and the policy, and
/// with nothing from the packer's own machine.
#[test]
fn a_sysroot_closure_is_bundled_and_pruned() {
    let td = workdir("sysroot");
    let Some(fixture) = synthetic_sysroot(&td) else {
        return;
    };
    let dir = td.join("app");
    std::fs::create_dir_all(&dir).unwrap();
    sysroot_recipe(&dir, &fixture, "");

    let out = onelf_build(&dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "build failed:\n{stderr}");
    assert!(dir.join("bin/app").is_file());
    assert!(dir.join("lib/libfixture.so.1").is_file());
    assert!(
        dir.join("lib/fixture/plugins/a.so").is_file(),
        "a plugin directory no ELF names still arrives"
    );
    assert!(dir.join("share/fixture/data.txt").is_file());
    assert!(
        !dir.join("lib/extra").exists(),
        "an unnamed optional dependency stays out"
    );
    assert!(
        !dir.join("lib/libGL.so.1").exists(),
        "the platform line keeps the driver out"
    );
    assert!(
        stderr.contains("Host-provided:") && stderr.contains("libGL.so.1"),
        "the host-provided library is reported:\n{stderr}"
    );
    assert!(
        !dir.join("share/doc").exists(),
        "the policy removes documentation"
    );
    assert!(
        td.join("sysroot/var/lib/pacman/local").is_dir(),
        "the archive was materialized"
    );
    assert_bundle_is_from_sysroot(&dir, &fixture.rootfs);

    // Naming the optional dependency brings it in.
    sysroot_recipe(&dir, &fixture, "optional = [\"extra\"]\n");
    let out = onelf_build(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.join("lib/extra/libextra.so").is_file());

    // The package runs, with the driver coming from the host through the
    // resolver as the platform line promised.
    let pkg = dir.join("app.onelf");
    let mut run = Command::new(&pkg);
    run.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .env("ONELF_LD_CACHE", &fixture.ld_cache);
    isolate(&mut run, &td);
    let out = run_package(&mut run);
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("value=42"),
        "the packed app runs: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The same closure from the command line, and the sysroot commands.
    let cli_dir = td.join("cli");
    std::fs::create_dir_all(&cli_dir).unwrap();
    let out = Command::new(onelf())
        .args([
            "bundle-libs",
            cli_dir.to_str().unwrap(),
            "--target",
            "bin/app",
        ])
        .args(["--sysroot", td.join("sysroot").to_str().unwrap()])
        .args(["--platform-line", fixture.platform_line.to_str().unwrap()])
        .args(["--policy", fixture.policy.to_str().unwrap()])
        .args(["--sysroot-optional", "extra"])
        .output()
        .expect("spawn onelf bundle-libs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(cli_dir.join("bin/app").is_file() && cli_dir.join("lib/extra/libextra.so").is_file());

    let out = Command::new(onelf())
        .args(["sysroot", "info", td.join("sysroot").to_str().unwrap()])
        .output()
        .expect("spawn onelf sysroot info");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success()
            && stdout.contains("packages: 5")
            && stdout.contains("glibc:    2.99-1"),
        "{stdout}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A dependency the sysroot cannot supply fails the build in sysroot mode
/// and only warns in host-scan mode.
#[test]
fn an_unresolved_soname_fails_the_sysroot_build_and_warns_the_host_scan() {
    let td = workdir("sysrootfail");
    let Some(fixture) = synthetic_sysroot(&td) else {
        return;
    };
    let dir = td.join("app");
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(onelf())
        .args(["sysroot", "fetch"])
        .arg(&fixture.archive)
        .arg(td.join("sysroot"))
        .output()
        .expect("spawn onelf sysroot fetch");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_file(td.join("sysroot/usr/lib/libfixture.so.1")).unwrap();
    sysroot_recipe(&dir, &fixture, "");

    let out = onelf_build(&dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "sysroot mode must fail:\n{stderr}");
    assert!(
        stderr.contains("libfixture.so.1") && stderr.contains("bin/app"),
        "the failure names the soname and the object:\n{stderr}"
    );

    let out = Command::new(onelf())
        .args(["bundle-libs", dir.to_str().unwrap(), "--target", "bin/app"])
        .env("ONELF_LD_CACHE", &fixture.ld_cache)
        .output()
        .expect("spawn onelf bundle-libs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "host-scan mode only warns:\n{stderr}");
    assert!(
        stderr.contains("libfixture.so.1"),
        "the warning names it:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A trace removes what the test run never opened, keeps the siblings of
/// what it did, and keeps what some bundled object needs.
#[test]
fn a_trace_prunes_unopened_files_but_keeps_siblings_and_needs() {
    let td = workdir("sysroottrace");
    let Some(fixture) = synthetic_sysroot(&td) else {
        return;
    };
    let dir = td.join("app");
    std::fs::create_dir_all(&dir).unwrap();
    let trace = td.join("trace.txt");
    write(&trace, "/usr/bin/app\n/usr/lib/fixture/plugins/a.so\n");
    sysroot_recipe(
        &dir,
        &fixture,
        &format!("trace = \"{}\"\n", trace.display()),
    );

    let out = onelf_build(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !dir.join("share/fixture/data.txt").exists(),
        "unopened data is removed"
    );
    assert!(
        dir.join("lib/fixture/plugins/b.so").is_file(),
        "an opened plugin's sibling stays"
    );
    assert!(
        dir.join("lib/libfixture.so.1").is_file(),
        "a needed soname stays"
    );
    assert!(dir.join("lib/libc.so.6").is_file());

    let plain = td.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    sysroot_recipe(&plain, &fixture, "");
    let out = onelf_build(&plain);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        plain.join("share/fixture/data.txt").is_file(),
        "no trace, nothing pruned"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A raw ustar header for one small file, for archives `tar` itself
/// refuses to write.
fn raw_tar_entry(name: &str, data: &[u8]) -> Vec<u8> {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..107].copy_from_slice(b"0000644");
    header[108..115].copy_from_slice(b"0000000");
    header[116..123].copy_from_slice(b"0000000");
    header[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
    header[136..147].copy_from_slice(b"00000000000");
    header[156] = b'0';
    header[257..262].copy_from_slice(b"ustar");
    header[263..265].copy_from_slice(b"00");
    header[148..156].copy_from_slice(b"        ");
    let sum: u32 = header.iter().map(|&b| b as u32).sum();
    header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    let mut out = header.to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out.extend_from_slice(&[0u8; 1024]);
    out
}

/// The same archive and recipe give the same bytes, and an archive that
/// reaches outside its directory is refused.
#[test]
fn sysroot_builds_are_reproducible_and_traversal_is_refused() {
    let td = workdir("sysrootrepro");
    let Some(fixture) = synthetic_sysroot(&td) else {
        return;
    };
    let mut outputs = Vec::new();
    for name in ["one", "two"] {
        let dir = td.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        sysroot_recipe(&dir, &fixture, "");
        let out = onelf_build(&dir);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        outputs.push(std::fs::read(dir.join("app.onelf")).unwrap());
    }
    assert!(outputs[0] == outputs[1], "two builds differ");

    let bad = td.join("bad.tar");
    std::fs::write(&bad, raw_tar_entry("../escape", b"x")).unwrap();
    let out = Command::new(onelf())
        .args(["sysroot", "fetch"])
        .arg(&bad)
        .arg(td.join("badroot"))
        .output()
        .expect("spawn onelf sysroot fetch");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("escape"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!td.join("escape").exists());

    let _ = std::fs::remove_dir_all(&td);
}

/// A bundle built from a sysroot records what it came from, and `onelf
/// info` reports it; a host-scan bundle reports that nothing is recorded.
#[test]
fn a_sysroot_build_records_its_provenance() {
    let td = workdir("provenance");
    let Some(fixture) = synthetic_sysroot(&td) else {
        return;
    };
    let dir = td.join("app");
    std::fs::create_dir_all(&dir).unwrap();
    sysroot_recipe(&dir, &fixture, "platform = \"platform-test\"\n");
    let out = onelf_build(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pkg = dir.join("app.onelf");

    let list = Command::new(onelf())
        .args(["list", pkg.to_str().unwrap()])
        .output()
        .expect("spawn onelf list");
    assert!(String::from_utf8_lossy(&list.stdout).contains(".onelf/provenance.toml"));

    let info = Command::new(onelf())
        .args(["info", pkg.to_str().unwrap()])
        .output()
        .expect("spawn onelf info");
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(info.status.success());
    assert!(stdout.contains("Platform:     platform-test"), "{stdout}");
    for line in [
        "app 1.0-1",
        "glibc 2.99-1",
        "libfixture 1.0-1",
        "mesa-fake 1.0-1",
    ] {
        assert!(stdout.contains(line), "{line} missing:\n{stdout}");
    }
    assert!(
        !stdout.contains("extra 1.0-1"),
        "an unnamed optional package is not listed"
    );

    let plain = pack_script(&td, "plain", "#!/bin/sh\necho hi\n");
    let info = Command::new(onelf())
        .args(["info", plain.to_str().unwrap()])
        .output()
        .expect("spawn onelf info");
    assert!(String::from_utf8_lossy(&info.stdout).contains("none recorded"));

    let _ = std::fs::remove_dir_all(&td);
}

/// The sysroot's pin travels into the package as `.onelf/platform`,
/// under the package's label, with the recipe overriding it field by
/// field; a sysroot without the file yields no pin.
#[test]
fn a_sysroot_pin_is_carried_into_the_package() {
    let td = workdir("pin");
    let Some(fixture) = synthetic_sysroot(&td) else {
        return;
    };
    let dir = td.join("app");
    std::fs::create_dir_all(&dir).unwrap();
    sysroot_recipe(&dir, &fixture, "platform = \"platform-test\"\n");
    let out = onelf_build(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("GL build:"), "{stderr}");
    let record = std::fs::read_to_string(dir.join(".onelf/platform")).unwrap();
    assert_eq!(
        record,
        format!(
            "label = \"platform-test\"\nurl = \"file://{}\"\nblake3 = \"{}\"\n",
            fixture.gl_build.display(),
            fixture.gl_hash
        )
    );

    let pkg = dir.join("app.onelf");
    let list = Command::new(onelf())
        .args(["list", pkg.to_str().unwrap()])
        .output()
        .expect("spawn onelf list");
    assert!(String::from_utf8_lossy(&list.stdout).contains(".onelf/platform"));
    let info = Command::new(onelf())
        .args(["info", pkg.to_str().unwrap()])
        .output()
        .expect("spawn onelf info");
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(stdout.contains("GL build:"), "{stdout}");
    assert!(stdout.contains(&fixture.gl_hash), "{stdout}");

    sysroot_recipe(
        &dir,
        &fixture,
        "platform = \"platform-test\"\nplatform-url = \"file:///elsewhere/gl.onelf\"\n",
    );
    let out = onelf_build(&dir);
    assert!(out.status.success());
    let record = std::fs::read_to_string(dir.join(".onelf/platform")).unwrap();
    assert!(
        record.contains("url = \"file:///elsewhere/gl.onelf\""),
        "{record}"
    );
    assert!(
        record.contains(&fixture.gl_hash),
        "the sysroot's hash stays"
    );

    sysroot_recipe(
        &dir,
        &fixture,
        "platform = \"platform-test\"\nplatform-url = \"http://insecure/gl.onelf\"\n",
    );
    let out = onelf_build(&dir);
    assert!(!out.status.success(), "a plain http pin is refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("https://"));

    std::fs::remove_file(td.join("sysroot/etc/onelf/platform.toml")).unwrap();
    sysroot_recipe(&dir, &fixture, "platform = \"platform-test\"\n");
    let out = onelf_build(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.join(".onelf/platform").exists());
    let list = Command::new(onelf())
        .args(["list", pkg.to_str().unwrap()])
        .output()
        .expect("spawn onelf list");
    assert!(!String::from_utf8_lossy(&list.stdout).contains(".onelf/platform"));

    let _ = std::fs::remove_dir_all(&td);
}

/// The record is display only: an edited one changes nothing about how
/// the package runs, and builds stay reproducible with it in place.
#[test]
fn an_edited_provenance_record_changes_nothing() {
    let td = workdir("provenanceedit");
    let Some(fixture) = synthetic_sysroot(&td) else {
        return;
    };
    let out = Command::new(onelf())
        .args(["sysroot", "fetch"])
        .arg(&fixture.archive)
        .arg(td.join("sysroot"))
        .output()
        .expect("spawn onelf sysroot fetch");
    assert!(out.status.success());

    let bundle = |dir: &Path| {
        std::fs::create_dir_all(dir).unwrap();
        let out = Command::new(onelf())
            .args(["bundle-libs", dir.to_str().unwrap(), "--target", "bin/app"])
            .args(["--sysroot", td.join("sysroot").to_str().unwrap()])
            .args(["--platform-line", fixture.platform_line.to_str().unwrap()])
            .args(["--policy", fixture.policy.to_str().unwrap()])
            .output()
            .expect("spawn onelf bundle-libs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let pack = |dir: &Path, out: &Path| {
        let o = Command::new(onelf())
            .args(["pack", dir.to_str().unwrap(), "-o", out.to_str().unwrap()])
            .args(["--command", "bin/app", "--mtime", "0"])
            .output()
            .expect("spawn onelf pack");
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    let run = |pkg: &Path| {
        let mut run = Command::new(pkg);
        run.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", td.to_str().unwrap())
            .env("ONELF_LD_CACHE", &fixture.ld_cache);
        isolate(&mut run, &td);
        let out = run_package(&mut run);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let honest = td.join("honest");
    bundle(&honest);
    pack(&honest, &td.join("honest.onelf"));
    let expected = run(&td.join("honest.onelf"));

    let edited = td.join("edited");
    bundle(&edited);
    std::fs::write(
        edited.join(".onelf/provenance.toml"),
        "platform = \"lies\"\n\n[[package]]\nname = \"nothing-here\"\nversion = \"9\"\n",
    )
    .unwrap();
    pack(&edited, &td.join("edited.onelf"));
    assert_eq!(run(&td.join("edited.onelf")), expected);

    let again = td.join("again");
    bundle(&again);
    pack(&again, &td.join("again.onelf"));
    assert!(
        std::fs::read(td.join("honest.onelf")).unwrap()
            == std::fs::read(td.join("again.onelf")).unwrap(),
        "two sysroot builds with their records differ"
    );

    let _ = std::fs::remove_dir_all(&td);
}

/// A record that does not parse is reported as malformed, and the rest
/// of the package's information still prints.
#[test]
fn a_malformed_provenance_record_is_reported_not_fatal() {
    let td = workdir("provenancebad");
    let Some(fixture) = synthetic_sysroot(&td) else {
        return;
    };
    let out = Command::new(onelf())
        .args(["sysroot", "fetch"])
        .arg(&fixture.archive)
        .arg(td.join("sysroot"))
        .output()
        .expect("spawn onelf sysroot fetch");
    assert!(out.status.success());
    let dir = td.join("app");
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(onelf())
        .args(["bundle-libs", dir.to_str().unwrap(), "--target", "bin/app"])
        .args(["--sysroot", td.join("sysroot").to_str().unwrap()])
        .args(["--platform-line", fixture.platform_line.to_str().unwrap()])
        .output()
        .expect("spawn onelf bundle-libs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let record = dir.join(".onelf/provenance.toml");
    let text = std::fs::read_to_string(&record).unwrap();
    std::fs::write(&record, &text[..text.len() - 6]).unwrap();
    let pkg = td.join("app.onelf");
    let o = Command::new(onelf())
        .args(["pack", dir.to_str().unwrap(), "-o", pkg.to_str().unwrap()])
        .args(["--command", "bin/app", "--mtime", "0"])
        .output()
        .expect("spawn onelf pack");
    assert!(o.status.success());

    let info = Command::new(onelf())
        .args(["info", pkg.to_str().unwrap()])
        .output()
        .expect("spawn onelf info");
    let stdout = String::from_utf8_lossy(&info.stdout);
    assert!(info.status.success());
    assert!(stdout.contains("malformed record"), "{stdout}");
    assert!(stdout.contains("Entrypoints:"), "{stdout}");

    let _ = std::fs::remove_dir_all(&td);
}
