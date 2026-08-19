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
    let out = run.output().expect("run corrupt package");
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
    let st = run.status().expect("run package");

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
    let st = run.status().expect("run package");
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
    let st = run.status().expect("run package (empty PATH)");
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
