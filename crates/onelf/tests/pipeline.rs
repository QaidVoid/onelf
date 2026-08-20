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
    if !have("fusermount3") {
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

/// FUSE must stream an entry, not buffer it.
///
/// The proof is an address-space limit well below the entry size: if any
/// stage held the whole entry, the allocation would fail. A 300 MB entry
/// of zeros packs to under 1 MB, so this costs the suite a fraction of a
/// second rather than a large fixture.
#[test]
fn a_large_entry_reads_under_an_address_space_limit() {
    if !have("fusermount3") {
        return; // documented soft-skip, as with cc and patchelf
    }
    let td = workdir("fusemem");
    let app = td.join("app");
    std::fs::create_dir_all(app.join("bin")).unwrap();
    write(
        &app.join("bin/run"),
        "#!/bin/sh\nwc -c < \"$ONELF_DIR/big.bin\"\n",
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
