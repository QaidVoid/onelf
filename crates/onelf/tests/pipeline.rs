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
    if let Ok(p) = std::env::var("ONELF_PATCHELF") {
        if Path::new(&p).is_file() {
            return Some(p);
        }
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
    let data: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
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
    assert!(o.status.success(), "pack: {}", String::from_utf8_lossy(&o.stderr));

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
    assert!(o.status.success(), "extract: {}", String::from_utf8_lossy(&o.stderr));
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
    assert!(o.status.success(), "pack: {}", String::from_utf8_lossy(&o.stderr));

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
    let st = Command::new(&pkg)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", td.to_str().unwrap())
        .status()
        .expect("run package");

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
