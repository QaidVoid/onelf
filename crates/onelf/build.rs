use std::env;
use std::path::PathBuf;
use std::process::Command;

fn musl_target() -> String {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| {
        if cfg!(target_arch = "aarch64") {
            "aarch64".to_string()
        } else {
            "x86_64".to_string()
        }
    });
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
        if let Ok(output) = Command::new("which").arg(name).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(path);
                }
            }
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
    println!("cargo:rerun-if-env-changed=ONELF_MUSL_CC");
    println!("cargo:rerun-if-env-changed={cc_env}");

    // Allow pre-built runtime via env var (needed for cargo publish/install)
    if let Ok(rt_path) = env::var("ONELF_RT_PATH") {
        let path = PathBuf::from(&rt_path);
        if !path.exists() {
            panic!("ONELF_RT_PATH={rt_path} does not exist");
        }
        println!("cargo:rustc-env=ONELF_RT_PATH={rt_path}");
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

    let target_dir = PathBuf::from(&out_dir).join("onelf-rt-build");

    // Find musl CC
    let musl_cc = find_musl_gcc(&target).unwrap_or_else(|| {
        let cc_env = format!("CC_{}", target.replace('-', "_"));
        panic!(
            "Could not find musl-gcc for {target}. Set ONELF_MUSL_CC or {cc_env}, \
             or install musl-gcc to PATH.",
        )
    });
    eprintln!("Using musl CC: {musl_cc}");

    // Build onelf-rt for musl
    let mut cmd = Command::new(&cargo);

    // Clean cargo env vars to avoid interference, but preserve linker settings
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
        .env("CC", &musl_cc)
        .env(format!("CC_{}", target.replace('-', "_")), &musl_cc)
        .current_dir(&rt_dir)
        .arg("build")
        .arg("--target")
        .arg(&target)
        .arg("--target-dir")
        .arg(&target_dir);

    if profile == "release" {
        cmd.arg("--release");
    }

    eprintln!("Building onelf-rt for {target}...");
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to build onelf-rt: {e}"));

    if !status.success() {
        panic!("onelf-rt build failed");
    }

    let rt_binary = target_dir.join(target).join(&profile).join("onelf-rt");

    if !rt_binary.exists() {
        panic!("onelf-rt binary not found at: {}", rt_binary.display());
    }

    println!("cargo:rustc-env=ONELF_RT_PATH={}", rt_binary.display());
}
