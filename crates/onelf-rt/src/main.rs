mod cache;
mod env;
mod ephemeral;
mod fuse;
mod interp;
mod loader;
mod memfd;
mod metadata;
mod multicall;
mod portable;
mod ulexec;
#[cfg(feature = "update")]
mod update;

use std::os::unix::process::CommandExt;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().map(|s| s.as_str()).unwrap_or("onelf");

    let exec_path = std::fs::read_link("/proc/self/exe")
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();

    let exe_path = std::path::Path::new(&exec_path);
    let exe_dir = exe_path.parent().unwrap_or(std::path::Path::new("."));
    let exe_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("onelf");

    // Handle --onelf-portable-* flags (create dirs and exit)
    if portable::handle_portable_flags(&args, exe_dir, exe_name) {
        return;
    }

    let mut pkg = match loader::load() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("onelf-rt: failed to load package: {e}");
            std::process::exit(1);
        }
    };

    let ep_idx = multicall::resolve_entrypoint(&pkg.manifest, argv0);

    if ep_idx >= pkg.manifest.entrypoints.len() {
        eprintln!("onelf-rt: no valid entrypoint found");
        std::process::exit(1);
    }

    let ep_name = pkg
        .manifest
        .get_string(pkg.manifest.entrypoints[ep_idx].name)
        .to_string();

    // Handle --onelf-icon / --onelf-desktop before dispatching
    if metadata::handle_metadata_flags(&args, &mut pkg, &ep_name) {
        return;
    }

    // Handle --onelf-update / --onelf-check-update (only when built with the
    // "update" feature; the slim runtime omits these to save ~1.3 MB).
    #[cfg(feature = "update")]
    if let Some(flag) = update::parse_flag(&args) {
        let Some(url_bytes) = read_package_file(&mut pkg, ".onelf/update-url") else {
            eprintln!("onelf-rt: no update URL configured (repack with --update-url)");
            std::process::exit(1);
        };
        let url = match std::str::from_utf8(&url_bytes) {
            Ok(s) => s.trim().to_string(),
            Err(_) => {
                eprintln!("onelf-rt: update URL is not valid UTF-8");
                std::process::exit(1);
            }
        };
        let Some(self_path) = update::self_path() else {
            eprintln!("onelf-rt: cannot resolve /proc/self/exe");
            std::process::exit(1);
        };
        std::process::exit(update::run(flag, &self_path, &url));
    }

    let ep_target_entry = pkg.manifest.entrypoints[ep_idx].target_entry as usize;
    let ep_working_dir = pkg.manifest.entrypoints[ep_idx].working_dir;
    let ep_memfd = pkg.manifest.entrypoints[ep_idx].is_memfd_eligible();

    let target_blocks = pkg.manifest.entries[ep_target_entry].blocks.clone();

    let ep_args_str = pkg
        .manifest
        .get_string(pkg.manifest.entrypoints[ep_idx].args)
        .to_string();
    let extra_args: Vec<String> = if ep_args_str.is_empty() {
        Vec::new()
    } else {
        ep_args_str.split('\x1f').map(String::from).collect()
    };

    // Build final args: extra_args + remaining argv (skip argv[0])
    let mut final_args = extra_args;
    if args.len() > 1 {
        final_args.extend_from_slice(&args[1..]);
    }

    // ONELF_MODE: "memfd", "fuse", or "cache" to force a specific mode.
    // Default order: memfd (if eligible) -> fuse -> cache
    let forced_mode = std::env::var("ONELF_MODE").ok();
    let force = forced_mode.as_deref();

    // Memfd mode: single static binary, no libs needed
    if force == Some("memfd") || (force.is_none() && ep_memfd) {
        if let Ok(data) = loader::read_payload_blocks(
            &mut pkg.file,
            pkg.footer.payload_offset,
            &target_blocks,
            pkg.dict.as_deref(),
        ) {
            let lib_paths_str = pkg.manifest.lib_dirs().join(":");
            // memfd mode: target is the memfd data itself (an ELF we
            // just read). Pass a non-empty marker so setup_env treats
            // it as an ELF.
            env::setup_env(
                "",
                argv0,
                &exec_path,
                &ep_name,
                "memfd",
                &lib_paths_str,
                "/proc/self/fd/0",
            );
            portable::setup_portable(exe_dir, exe_name);

            if let Err(e) = memfd::execute_memfd(&data, argv0, &final_args) {
                if force == Some("memfd") {
                    eprintln!("onelf-rt: memfd execution failed: {e}");
                    std::process::exit(1);
                }
            }
        } else if force == Some("memfd") {
            eprintln!("onelf-rt: failed to read payload for memfd");
            std::process::exit(1);
        }
    }

    // Read interpreter metadata for cross-libc portability (if packed with interp patching)
    let interp_data = read_package_file(&mut pkg, ".onelf/interp");

    // Read custom environment variables from recipe [env] section
    let env_data = read_package_file(&mut pkg, ".onelf/env");

    // FUSE mode: mount package as filesystem (default for non-memfd)
    if force != Some("cache") && force != Some("tmpfs") {
        fuse::execute_fuse(
            &mut pkg,
            ep_idx,
            argv0,
            &exec_path,
            &final_args,
            interp_data.as_deref(),
            env_data.as_deref(),
        );
        // Only reaches here if FUSE fell back
        if force == Some("fuse") {
            eprintln!("onelf-rt: FUSE mode unavailable");
            std::process::exit(1);
        }
    }

    // Ephemeral tmpfs mode: private namespace + tmpfs + extract. Invisible
    // to the host, no persistent on-disk artifacts. Preferred over cache
    // mode whenever user namespaces are available.
    if force != Some("cache") {
        ephemeral::execute_tmpfs(
            &mut pkg,
            ep_idx,
            argv0,
            &exec_path,
            &final_args,
            interp_data.as_deref(),
            env_data.as_deref(),
        );
        if force == Some("tmpfs") {
            eprintln!("onelf-rt: tmpfs mode unavailable");
            std::process::exit(1);
        }
    }

    // Persistent cache extraction mode (final fallback)
    let pkg_dir = match cache::ensure_extracted(&mut pkg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("onelf-rt: extraction failed: {e}");
            std::process::exit(1);
        }
    };

    let package_id = cache::hex(&pkg.manifest.header.package_id);
    let cache_base = cache::base_dir();

    // Auto-GC: prune stale cache entries
    let gc_max_age = std::env::var("ONELF_GC_MAX_AGE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30);
    if gc_max_age > 0 {
        cache::auto_gc(&cache_base, gc_max_age * 86400, &package_id);
    }

    let target_path_str = pkg.manifest.entry_path(ep_target_entry);
    let target_path = pkg_dir.join(&target_path_str);

    if !target_path.exists() {
        eprintln!(
            "onelf-rt: entrypoint target does not exist: {}",
            target_path.display()
        );
        std::process::exit(1);
    }

    let pkg_dir_str = pkg_dir.to_str().unwrap_or("");
    let lib_paths_str = pkg.manifest.lib_dirs().join(":");
    let target_path_s = target_path.to_str().unwrap_or("");
    env::setup_env(
        pkg_dir_str,
        argv0,
        &exec_path,
        &ep_name,
        "cache",
        &lib_paths_str,
        target_path_s,
    );
    if let Some(data) = &env_data {
        env::apply_custom_env(data, pkg_dir_str);
    }
    portable::setup_portable(exe_dir, exe_name);

    // Handle working directory
    match ep_working_dir {
        onelf_format::WorkingDir::PackageRoot => {
            let _ = std::env::set_current_dir(&pkg_dir);
        }
        onelf_format::WorkingDir::EntrypointParent => {
            if let Some(parent) = target_path.parent() {
                let _ = std::env::set_current_dir(parent);
            }
        }
        onelf_format::WorkingDir::Inherit => {}
    }

    let lib_dirs = pkg.manifest.lib_dirs();
    let bundled_interp_rel = interp_data
        .as_deref()
        .and_then(interp::parse_bundled_interp_rel);

    if let Some(interp) =
        interp::should_use_userland_exec(&target_path, &pkg_dir, bundled_interp_rel)
    {
        interp::exec_userland(&target_path, &interp, argv0, &final_args);
    }

    let mut cmd =
        interp::build_exec_command(&target_path, &pkg_dir, &lib_dirs, argv0, &final_args);

    let err = cmd.exec();

    eprintln!("onelf-rt: exec failed: {err}");
    std::process::exit(1);
}

fn read_package_file(pkg: &mut loader::PackageData, path: &str) -> Option<Vec<u8>> {
    let idx = (0..pkg.manifest.entries.len()).find(|&i| {
        pkg.manifest.entries[i].kind == onelf_format::EntryKind::File
            && pkg.manifest.entry_path(i) == path
    })?;
    loader::read_payload_blocks(
        &mut pkg.file,
        pkg.footer.payload_offset,
        &pkg.manifest.entries[idx].blocks,
        pkg.dict.as_deref(),
    )
    .ok()
}
