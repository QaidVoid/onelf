use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve `name` against `PATH`, or take it as given when it already names a
/// path. Stands in for shelling out to `which`, which minimal build images
/// (the pkgforge Arch container, most `-slim` bases) don't ship.
fn which(name: &str) -> Option<PathBuf> {
    let named = Path::new(name);
    if named.components().count() > 1 {
        return is_executable(named).then(|| named.to_path_buf());
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn musl_target() -> String {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| {
        if cfg!(target_arch = "aarch64") {
            "aarch64".to_string()
        } else {
            "x86_64".to_string()
        }
    });
    // rustc reports 32-bit x86 as `x86`; the musl triple uses `i686`.
    let arch = if arch == "x86" { "i686" } else { &arch };
    format!("{arch}-unknown-linux-musl")
}

fn find_musl_gcc(target: &str) -> Option<String> {
    let cc_env = format!("CC_{}", target.replace('-', "_"));

    // Check explicit env override
    if let Ok(cc) = env::var("ONELF_MUSL_CC") {
        return Some(cc);
    }
    if let Ok(cc) = env::var(&cc_env) {
        return Some(cc);
    }

    // Try architecture-specific and generic names in PATH
    let arch = target.split('-').next().unwrap_or("x86_64");
    let names = [format!("{arch}-linux-musl-gcc"), "musl-gcc".to_string()];
    for name in &names {
        if let Some(path) = which(name) {
            return Some(path.to_string_lossy().into_owned());
        }
    }

    // Bootlin prebuilt musl toolchains under /opt/bootlin (bin/<arch>-linux-gcc).
    let bootlin_dir = match arch {
        "x86_64" => Some("x86-64-musl"),
        "aarch64" => Some("aarch64-musl"),
        "i686" => Some("x86-i686-musl"),
        _ => None,
    };
    if let Some(dir) = bootlin_dir {
        let p = format!("/opt/bootlin/{dir}/bin/{arch}-linux-gcc");
        if Path::new(&p).exists() {
            return Some(p);
        }
    }

    // Search in /nix/store for musl-gcc (NixOS)
    if let Ok(entries) = std::fs::read_dir("/nix/store") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.contains("musl") && name_str.contains("-dev") {
                let gcc_path = entry.path().join("bin/musl-gcc");
                if gcc_path.exists() {
                    return Some(gcc_path.to_string_lossy().to_string());
                }
            }
        }
    }

    None
}

fn main() {
    let target = musl_target();
    let cc_env = format!("CC_{}", target.replace('-', "_"));

    println!("cargo:rerun-if-env-changed=ONELF_RT_PATH");
    println!("cargo:rerun-if-env-changed=ONELF_RT_UPDATE_PATH");
    println!("cargo:rerun-if-env-changed=ONELF_MUSL_CC");
    println!("cargo:rerun-if-env-changed={cc_env}");
    println!("cargo:rerun-if-env-changed=ONELF_PAYLOAD_DIR");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    // The freestanding bootstrap + env payloads (both arches) are always
    // embedded, independent of the runtime, so build them before the RT logic
    // (which may early-return on the ONELF_RT_PATH bypass).
    build_payloads();

    // Allow pre-built runtimes via env var (needed for cargo publish /
    // cargo install and CI builds that skip the musl toolchain). Both
    // the slim and update-capable variants must be wired. If only
    // ONELF_RT_PATH is set, reuse it for the update path too; packages
    // configured for self-update won't ship a separate update-capable
    // runtime in that case, but everything else still compiles.
    if let Ok(rt_path) = env::var("ONELF_RT_PATH") {
        let path = PathBuf::from(&rt_path);
        if !path.exists() {
            panic!("ONELF_RT_PATH={rt_path} does not exist");
        }
        println!("cargo:rustc-env=ONELF_RT_PATH={rt_path}");

        let update_path = env::var("ONELF_RT_UPDATE_PATH").unwrap_or_else(|_| rt_path.clone());
        if !PathBuf::from(&update_path).exists() {
            panic!("ONELF_RT_UPDATE_PATH={update_path} does not exist");
        }
        println!("cargo:rustc-env=ONELF_RT_UPDATE_PATH={update_path}");
        return;
    }

    println!("cargo:rerun-if-changed=../onelf-rt/src/");
    println!("cargo:rerun-if-changed=../onelf-format/src/");

    let out_dir = env::var("OUT_DIR").unwrap();
    let profile = env::var("PROFILE").unwrap();

    let cargo = PathBuf::from(env::var("CARGO").unwrap())
        .canonicalize()
        .unwrap();

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let rt_dir = manifest_dir.join("../onelf-rt");
    if !rt_dir.exists() {
        panic!(
            "onelf-rt source not found at {}. Set ONELF_RT_PATH to a pre-built runtime binary.",
            rt_dir.display()
        );
    }
    let rt_dir = rt_dir.canonicalize().unwrap();

    // Find musl CC
    let musl_cc = find_musl_gcc(&target).unwrap_or_else(|| {
        let cc_env = format!("CC_{}", target.replace('-', "_"));
        panic!(
            "Could not find musl-gcc for {target}. Set ONELF_MUSL_CC or {cc_env}, \
             or install musl-gcc to PATH.",
        )
    });
    eprintln!("Using musl CC: {musl_cc}");

    // Build the slim runtime (default features only).
    let slim = build_rt(
        &cargo,
        &rt_dir,
        &out_dir,
        &target,
        &profile,
        &musl_cc,
        "slim",
        &[],
    );
    println!("cargo:rustc-env=ONELF_RT_PATH={}", slim.display());

    // Build the update-capable runtime (pulls in rustls/ureq, ~1.3 MB extra).
    let full = build_rt(
        &cargo,
        &rt_dir,
        &out_dir,
        &target,
        &profile,
        &musl_cc,
        "update",
        &["update"],
    );
    println!("cargo:rustc-env=ONELF_RT_UPDATE_PATH={}", full.display());
}

// Threads one nested-build invocation; a parameter object would be used once.
#[allow(clippy::too_many_arguments)]
fn build_rt(
    cargo: &PathBuf,
    rt_dir: &PathBuf,
    out_dir: &str,
    target: &str,
    profile: &str,
    musl_cc: &str,
    variant: &str,
    features: &[&str],
) -> PathBuf {
    let target_dir = PathBuf::from(out_dir).join(format!("onelf-rt-{variant}"));

    let mut cmd = Command::new(cargo);

    // Clean cargo env vars to avoid interference, but preserve linker settings.
    for (key, _) in env::vars() {
        if (key.starts_with("CARGO") || key.starts_with("RUSTC")) && !key.ends_with("_LINKER") {
            cmd.env_remove(&key);
        }
    }

    let mut rustflags = String::from(
        "-Ctarget-feature=+crt-static -Crelocation-model=static -Clink-arg=-Wl,--no-dynamic-linker",
    );
    if profile == "release" {
        rustflags.push_str(" -Cdebuginfo=0");
    }

    cmd.env("RUSTFLAGS", &rustflags)
        .env("CC", musl_cc)
        .env(format!("CC_{}", target.replace('-', "_")), musl_cc)
        .current_dir(rt_dir)
        .arg("build")
        .arg("--target")
        .arg(target)
        .arg("--target-dir")
        .arg(&target_dir);

    if profile == "release" {
        cmd.arg("--release");
    }

    if !features.is_empty() {
        cmd.arg("--features").arg(features.join(","));
    }

    eprintln!("Building onelf-rt ({variant}) for {target}...");
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to build onelf-rt ({variant}): {e}"));

    if !status.success() {
        panic!("onelf-rt ({variant}) build failed");
    }

    let rt_binary = target_dir.join(target).join(profile).join("onelf-rt");
    if !rt_binary.exists() {
        panic!("onelf-rt binary not found at: {}", rt_binary.display());
    }
    rt_binary
}

struct Payload {
    arch: &'static str,
    triple: &'static str,
}

const PAYLOADS: &[Payload] = &[
    Payload {
        arch: "x86_64",
        triple: "x86_64-unknown-linux-gnu",
    },
    Payload {
        arch: "aarch64",
        triple: "aarch64-unknown-linux-gnu",
    },
    Payload {
        arch: "i686",
        triple: "i686-unknown-linux-gnu",
    },
];

/// Build (or collect) the freestanding bootstrap + env payloads and emit
/// `ONELF_BOOTSTRAP_<ARCH>` / `ONELF_ENV_<ARCH>` so `payload.rs` can
/// `include_bytes!` the artifacts from `OUT_DIR`.
///
/// By default only the payload for the arch being compiled is built: onelf's
/// embedded runtime is that arch, so packages it produces run on that arch and
/// never use another arch's bootstrap. The other arch gets an empty placeholder
/// (`bootstrap_blob` / `onelf_env_blob` return `None` for it). This keeps the
/// common build to a single toolchain. Set `ONELF_PAYLOAD_ALL=1` to build every
/// arch (needs each cross toolchain) for a cross-packing build.
fn build_payloads() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=../onelf-payloads/src");
    println!("cargo:rerun-if-changed=../onelf-payloads/bootstrap.ld");
    println!("cargo:rerun-if-changed=../onelf-payloads/Cargo.toml");
    println!("cargo:rerun-if-env-changed=ONELF_PAYLOAD_ALL");
    println!("cargo:rerun-if-env-changed=ONELF_PAYLOAD_CC_X86_64");
    println!("cargo:rerun-if-env-changed=ONELF_PAYLOAD_CC_AARCH64");
    println!("cargo:rerun-if-env-changed=ONELF_PAYLOAD_CC_I686");
    println!("cargo:rerun-if-env-changed=ONELF_OBJCOPY");

    // Escape hatch for toolchain-less builds (cargo publish / install, or CI
    // without the cross linkers): a directory holding prebuilt payloads. A
    // missing or empty entry is treated as "arch not provided" (placeholder),
    // so a single-arch prebuilt dir is fine.
    if let Ok(raw) = env::var("ONELF_PAYLOAD_DIR") {
        // Canonicalize to absolute: `include_bytes!` resolves the emitted path
        // relative to payload.rs, not the build CWD, so a relative dir would
        // fail to embed.
        let dir = std::fs::canonicalize(&raw)
            .unwrap_or_else(|e| panic!("ONELF_PAYLOAD_DIR={raw} is not accessible: {e}"));
        for p in PAYLOADS {
            let bs_src = dir.join(format!("bootstrap_{}.bin", p.arch));
            let env_src = dir.join(format!("onelf_env_{}.so", p.arch));
            // Regenerate when the prebuilt artifacts themselves change.
            println!("cargo:rerun-if-changed={}", bs_src.display());
            println!("cargo:rerun-if-changed={}", env_src.display());
            let bootstrap = collect_prebuilt(&out_dir, p.arch, &bs_src, false);
            let env_so = collect_prebuilt(&out_dir, p.arch, &env_src, true);
            emit_payload_env(p.arch, &bootstrap, &env_so);
        }
        return;
    }

    let target_arch =
        env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| std::env::consts::ARCH.to_string());
    // rustc's target_arch for 32-bit x86 is `x86`; PAYLOADS keys it as `i686`.
    let target_arch = if target_arch == "x86" {
        "i686".to_string()
    } else {
        target_arch
    };
    let build_all = env::var("ONELF_PAYLOAD_ALL").is_ok();

    let cargo = PathBuf::from(env::var("CARGO").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // Resolved lazily so a build targeting an arch onelf has no payload for
    // (every entry a placeholder) needs neither the payloads crate nor
    // llvm-tools.
    let mut payloads_dir: Option<PathBuf> = None;
    let mut objcopy: Option<PathBuf> = None;

    for p in PAYLOADS {
        if !build_all && p.arch != target_arch {
            let (bootstrap, env_so) = empty_placeholders(&out_dir, p.arch);
            emit_payload_env(p.arch, &bootstrap, &env_so);
            continue;
        }
        let pdir = payloads_dir.get_or_insert_with(|| {
            manifest_dir
                .join("../onelf-payloads")
                .canonicalize()
                .expect(
                    "onelf-payloads crate not found; set ONELF_PAYLOAD_DIR to prebuilt payloads",
                )
        });
        let oc = objcopy.get_or_insert_with(find_rust_objcopy);
        let cc = payload_cc(p.arch).unwrap_or_else(|| {
            panic!(
                "no linker for {arch} payloads. Set ONELF_PAYLOAD_CC_{up}, install \
                 an {triple} gcc, or set ONELF_PAYLOAD_DIR to prebuilt payloads.",
                arch = p.arch,
                up = p.arch.to_uppercase(),
                triple = p.triple,
            )
        });
        let bootstrap = build_bootstrap(&cargo, pdir, &out_dir, p, &cc, oc);
        let env_so = build_env(&cargo, pdir, &out_dir, p, &cc);
        emit_payload_env(p.arch, &bootstrap, &env_so);
    }
}

/// Write empty placeholder artifacts for an arch this build doesn't target.
/// `payload.rs`'s blob accessors treat a zero-length blob as absent.
fn empty_placeholders(out_dir: &Path, arch: &str) -> (PathBuf, PathBuf) {
    let bootstrap = out_dir.join(format!("bootstrap_{arch}.bin"));
    let env_so = out_dir.join(format!("onelf_env_{arch}.so"));
    std::fs::write(&bootstrap, []).unwrap();
    std::fs::write(&env_so, []).unwrap();
    (bootstrap, env_so)
}

fn emit_payload_env(arch: &str, bootstrap: &Path, env_so: &Path) {
    let a = arch.to_uppercase();
    println!(
        "cargo:rustc-env=ONELF_BOOTSTRAP_{a}={}",
        bootstrap.display()
    );
    println!("cargo:rustc-env=ONELF_ENV_{a}={}", env_so.display());
}

/// Resolve one escape-hatch payload. A present, non-empty file is validated
/// (env objects must be ELF) and used as-is; a missing or empty entry yields an
/// empty placeholder in `OUT_DIR` (that arch is simply not embedded). Returns
/// the path to `include_bytes!`.
fn collect_prebuilt(out_dir: &Path, arch: &str, path: &Path, is_elf: bool) -> PathBuf {
    let data = std::fs::read(path).unwrap_or_default();
    if data.is_empty() {
        let name = if is_elf {
            format!("onelf_env_{arch}.so")
        } else {
            format!("bootstrap_{arch}.bin")
        };
        let placeholder = out_dir.join(name);
        std::fs::write(&placeholder, []).unwrap();
        return placeholder;
    }
    if is_elf && (data.len() < 4 || &data[0..4] != b"\x7fELF") {
        panic!("ONELF_PAYLOAD_DIR: {} is not an ELF object", path.display());
    }
    path.to_path_buf()
}

/// Append reproducibility flags (path remaps) so no absolute `$CARGO_HOME` /
/// workspace path leaks into an embedded blob. Each flag stays a distinct
/// element: they are passed via `CARGO_ENCODED_RUSTFLAGS` (split on `\x1f`, not
/// whitespace) so a path containing spaces survives intact.
fn payload_rustflags(mut flags: Vec<String>) -> Vec<String> {
    if let Ok(home) = env::var("CARGO_HOME") {
        flags.push(format!("--remap-path-prefix={home}=/cargo"));
    }
    if let Ok(dir) = env::var("CARGO_MANIFEST_DIR")
        && let Some(ws) = PathBuf::from(&dir).parent().and_then(|p| p.parent())
    {
        flags.push(format!("--remap-path-prefix={}=/src", ws.display()));
    }
    flags
}

fn build_bootstrap(
    cargo: &Path,
    payloads_dir: &Path,
    out_dir: &Path,
    p: &Payload,
    cc: &str,
    objcopy: &Path,
) -> PathBuf {
    let target_dir = out_dir.join(format!("payload-bootstrap-{}", p.arch));
    let ld = payloads_dir.join("bootstrap.ld");
    // `pic`, not `static`: the flat binary is loaded at a runtime vaddr with no
    // loader to apply relocations, so all data access must be PC-relative
    // (`static` bakes in absolute addresses that would be wrong there). The
    // source avoids all `memcpy`/`memset` intrinsics, so `pic` introduces no
    // GOT-indirect calls (which would jump through a never-relocated slot).
    let flags = payload_rustflags(vec![
        "-Crelocation-model=pic".to_string(),
        format!("-Clinker={cc}"),
        "-Clink-arg=-nostdlib".to_string(),
        "-Clink-arg=-static".to_string(),
        format!("-Clink-arg=-Wl,-T,{}", ld.display()),
        "-Clink-arg=-Wl,-e,_onelf_start".to_string(),
        "-Clink-arg=-Wl,--build-id=none".to_string(),
    ]);
    run_payload_cargo(
        cargo,
        payloads_dir,
        &target_dir,
        p.triple,
        &flags,
        &["--bin", "onelf-bootstrap"],
    );
    let elf = target_dir.join(p.triple).join("release/onelf-bootstrap");
    let bin = out_dir.join(format!("bootstrap_{}.bin", p.arch));
    let status = Command::new(objcopy)
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin)
        .status()
        .unwrap_or_else(|e| panic!("failed to run objcopy for {} bootstrap: {e}", p.arch));
    if !status.success() {
        panic!("objcopy failed for {} bootstrap", p.arch);
    }
    bin
}

fn build_env(cargo: &Path, payloads_dir: &Path, out_dir: &Path, p: &Payload, cc: &str) -> PathBuf {
    let target_dir = out_dir.join(format!("payload-env-{}", p.arch));
    let flags = payload_rustflags(vec![
        "-Crelocation-model=pic".to_string(),
        format!("-Clinker={cc}"),
        "-Clink-arg=-nostdlib".to_string(),
        "-Clink-arg=-Wl,-soname,libonelf-env.so".to_string(),
        "-Clink-arg=-Wl,--build-id=none".to_string(),
    ]);
    run_payload_cargo(
        cargo,
        payloads_dir,
        &target_dir,
        p.triple,
        &flags,
        &["--lib"],
    );
    let so = target_dir.join(p.triple).join("release/libonelf_env.so");
    let dst = out_dir.join(format!("onelf_env_{}.so", p.arch));
    std::fs::copy(&so, &dst)
        .unwrap_or_else(|e| panic!("failed to stage {} env cdylib: {e}", p.arch));
    dst
}

/// Whether to strip `key` from the nested payload build's environment. Clears
/// the per-build Cargo/rustc context the parent injects (CARGO_MANIFEST_DIR,
/// CARGO_PKG_*, CARGO_CFG_*, CARGO_FEATURE_*, CARGO_ENCODED_RUSTFLAGS, and so
/// on) so it can't mis-configure the nested build, but keeps global
/// toolchain/registry config (CARGO_HOME, CARGO_NET_*, registry settings,
/// RUSTC and RUSTC_WRAPPER) and any `*_LINKER` override.
///
/// RUSTC_WORKSPACE_WRAPPER is deliberately NOT kept. It is how `cargo clippy`
/// injects clippy-driver for the crates it considers workspace members, and
/// the payloads crate is excluded from the workspace precisely because it is a
/// freestanding build with different rules. Letting it through would make
/// linting the outer workspace lint this crate too, and under `-D warnings`
/// that turns a lint into a build-script failure.
fn should_clear_for_nested(key: &str) -> bool {
    if !(key.starts_with("CARGO") || key.starts_with("RUSTC")) {
        return false;
    }
    if key.ends_with("_LINKER") {
        return false;
    }
    !matches!(key, "CARGO_HOME" | "RUSTC" | "RUSTC_WRAPPER")
        && !key.starts_with("CARGO_NET")
        && !key.starts_with("CARGO_REGISTR")
}

fn run_payload_cargo(
    cargo: &Path,
    dir: &Path,
    target_dir: &Path,
    triple: &str,
    flags: &[String],
    extra: &[&str],
) {
    let mut cmd = Command::new(cargo);
    for (key, _) in env::vars() {
        if should_clear_for_nested(&key) {
            cmd.env_remove(&key);
        }
    }
    // A stray inherited RUSTFLAGS would conflict with CARGO_ENCODED_RUSTFLAGS.
    cmd.env_remove("RUSTFLAGS");
    // `\x1f`-encoded so flags with spaces (paths) are not re-split.
    cmd.env("CARGO_ENCODED_RUSTFLAGS", flags.join("\x1f"))
        .current_dir(dir)
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg(triple)
        .arg("--target-dir")
        .arg(target_dir)
        .args(extra);
    if let Ok(epoch) = env::var("SOURCE_DATE_EPOCH") {
        cmd.env("SOURCE_DATE_EPOCH", epoch);
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to build payload ({triple}): {e}"));
    if !status.success() {
        panic!("payload build failed for {triple}");
    }
}

/// Find a C compiler to drive the linker for `arch`'s payloads. Honors
/// `ONELF_PAYLOAD_CC_<ARCH>`, then the host `cc` for the native arch, then a
/// Bootlin or `<triple>-gcc` cross compiler.
fn payload_cc(arch: &str) -> Option<String> {
    if let Ok(cc) = env::var(format!("ONELF_PAYLOAD_CC_{}", arch.to_uppercase())) {
        return Some(cc);
    }
    // Use the host `cc` only for a native Linux match: the payloads are Linux
    // ELF objects, so a non-Linux host `cc` (e.g. macOS clang) would emit the
    // wrong format. Otherwise fall through to an explicit cross compiler.
    let host = env::var("HOST").unwrap_or_default();
    if host.starts_with(arch) && host.contains("-linux") {
        return Some("cc".to_string());
    }
    let names: &[&str] = match arch {
        "x86_64" => &[
            "/opt/bootlin/x86-64-glibc/bin/x86_64-linux-gcc",
            "/opt/bootlin/x86-64-musl/bin/x86_64-linux-gcc",
            "x86_64-linux-gnu-gcc",
        ],
        "aarch64" => &[
            "/opt/bootlin/aarch64-glibc/bin/aarch64-linux-gcc",
            "/opt/bootlin/aarch64-musl/bin/aarch64-linux-gcc",
            "aarch64-linux-gnu-gcc",
        ],
        "i686" => &[
            "/opt/bootlin/x86-i686-glibc/bin/i686-linux-gcc",
            "/opt/bootlin/x86-i686-musl/bin/i686-linux-gcc",
            "i686-linux-gnu-gcc",
        ],
        _ => return None,
    };
    for name in names {
        if let Some(p) = which(name) {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

/// Locate `llvm-objcopy` / `rust-objcopy` from the installed `llvm-tools`.
fn find_rust_objcopy() -> PathBuf {
    if let Ok(p) = env::var("ONELF_OBJCOPY") {
        return PathBuf::from(p);
    }
    if let Ok(out) = Command::new("rustc").arg("--print").arg("sysroot").output()
        && out.status.success()
    {
        let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let host = env::var("HOST").unwrap_or_default();
        let cand = PathBuf::from(&sysroot)
            .join("lib/rustlib")
            .join(&host)
            .join("bin/llvm-objcopy");
        if cand.exists() {
            return cand;
        }
    }
    for name in ["rust-objcopy", "llvm-objcopy"] {
        if let Some(p) = which(name) {
            return p;
        }
    }
    panic!("llvm-objcopy not found; run `rustup component add llvm-tools` or set ONELF_OBJCOPY");
}
