mod cache;
mod env;
mod loader;
mod memfd;
mod multicall;

use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().map(|s| s.as_str()).unwrap_or("onelf");

    let exec_path = std::fs::read_link("/proc/self/exe")
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();

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

    if ep_memfd {
        let data = match loader::read_payload_blocks(
            &mut pkg.file,
            pkg.footer.payload_offset,
            &target_blocks,
            pkg.dict.as_deref(),
        ) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("onelf-rt: failed to read payload: {e}");
                std::process::exit(1);
            }
        };

        let lib_paths_str = pkg.manifest.lib_dirs().join(":");
        env::setup_env("", argv0, &exec_path, &ep_name, "memfd", &lib_paths_str);

        if let Err(e) = memfd::execute_memfd(&data, argv0, &final_args) {
            eprintln!("onelf-rt: memfd execution failed: {e}");
            std::process::exit(1);
        }
    }

    // Cache extraction mode
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
    env::setup_env(
        pkg_dir_str,
        argv0,
        &exec_path,
        &ep_name,
        "cache",
        &lib_paths_str,
    );

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

    let err = Command::new(&target_path)
        .arg0(argv0)
        .args(&final_args)
        .exec();

    eprintln!("onelf-rt: exec failed: {err}");
    std::process::exit(1);
}
