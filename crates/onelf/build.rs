use std::env;
use std::path::PathBuf;
use std::process::Command;

fn find_musl_gcc() -> Option<String> {
    // Check explicit env override
    if let Ok(cc) = env::var("ONELF_MUSL_CC") {
        return Some(cc);
    }
    if let Ok(cc) = env::var("CC_x86_64_unknown_linux_musl") {
        return Some(cc);
    }

    // Try common names in PATH
    for name in &["x86_64-linux-musl-gcc", "musl-gcc"] {
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
    println!("cargo:rerun-if-changed=../onelf-rt/src/");
    println!("cargo:rerun-if-changed=../onelf-format/src/");
    println!("cargo:rerun-if-env-changed=ONELF_MUSL_CC");
    println!("cargo:rerun-if-env-changed=CC_x86_64_unknown_linux_musl");

    let out_dir = env::var("OUT_DIR").unwrap();
    let profile = env::var("PROFILE").unwrap();
    let target = "x86_64-unknown-linux-musl";

    let cargo = PathBuf::from(env::var("CARGO").unwrap())
        .canonicalize()
        .unwrap();

    let rt_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../onelf-rt")
        .canonicalize()
        .unwrap();

    let target_dir = PathBuf::from(&out_dir).join("onelf-rt-build");

    // Find musl CC
    let musl_cc = find_musl_gcc().expect(
        "Could not find musl-gcc. Set ONELF_MUSL_CC or CC_x86_64_unknown_linux_musl, \
         or install musl-gcc to PATH.",
    );
    eprintln!("Using musl CC: {musl_cc}");

    // Build onelf-rt for musl
    let mut cmd = Command::new(&cargo);

    // Clean cargo env vars to avoid interference
    for (key, _) in env::vars() {
        if key.starts_with("CARGO") || key.starts_with("RUSTC") {
            cmd.env_remove(&key);
        }
    }

    let mut rustflags = String::from("-Ctarget-feature=+crt-static");
    if profile == "release" {
        rustflags.push_str(" -Cdebuginfo=0");
    }

    cmd.env("RUSTFLAGS", &rustflags)
        .env("CC", &musl_cc)
        .env("CC_x86_64_unknown_linux_musl", &musl_cc)
        .current_dir(&rt_dir)
        .arg("build")
        .arg("--target")
        .arg(target)
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
