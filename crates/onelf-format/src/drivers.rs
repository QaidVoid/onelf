//! Host directories that supply GPU and system libraries.
//!
//! A bundled loader has its compiled-in search paths scrubbed, so anything
//! the host must still provide (libcuda, libvulkan, libGL, libva) has to be
//! named explicitly. The packed runtime and `onelf run` both need the same
//! list, and previously kept two copies in sync by comment.

/// Host driver and system library directories for `arch`, in descending
/// priority, filtered to those that exist.
///
/// `arch` takes the `std::env::consts::ARCH` spelling of the architecture
/// the package targets. Multiarch directory names are derived from it, so a
/// package is not handed x86_64 paths on aarch64.
pub fn host_driver_paths(arch: &str) -> Vec<String> {
    // NixOS exposes every GPU userspace driver under /run/opengl-driver,
    // populated from the active `hardware.graphics` closure. Elsewhere they
    // sit alongside the rest of the system's libraries.
    let mut dirs: Vec<&str> = vec!["/run/opengl-driver/lib", "/run/opengl-driver-32/lib"];
    dirs.extend(multiarch_dirs(arch));
    dirs.extend(["/usr/lib64", "/usr/lib", "/lib64", "/lib"]);

    let mut seen: Vec<String> = Vec::new();
    for d in dirs {
        if std::path::Path::new(d).is_dir() && !seen.iter().any(|s| s == d) {
            seen.push(d.to_string());
        }
    }
    seen
}

/// Debian-style multiarch library directories for `arch`, or nothing when the
/// architecture has no well-known tuple.
fn multiarch_dirs(arch: &str) -> &'static [&'static str] {
    match arch {
        "x86_64" => &["/usr/lib/x86_64-linux-gnu", "/lib/x86_64-linux-gnu"],
        "aarch64" => &["/usr/lib/aarch64-linux-gnu", "/lib/aarch64-linux-gnu"],
        "x86" => &["/usr/lib/i386-linux-gnu", "/lib/i386-linux-gnu"],
        "arm" => &["/usr/lib/arm-linux-gnueabihf", "/lib/arm-linux-gnueabihf"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiarch_follows_the_architecture() {
        assert_eq!(multiarch_dirs("x86_64")[0], "/usr/lib/x86_64-linux-gnu");
        assert_eq!(multiarch_dirs("aarch64")[0], "/usr/lib/aarch64-linux-gnu");
        assert_eq!(multiarch_dirs("x86")[0], "/usr/lib/i386-linux-gnu");
        assert!(multiarch_dirs("riscv64").is_empty());
    }

    #[test]
    fn results_exist_and_are_unique() {
        let dirs = host_driver_paths(std::env::consts::ARCH);
        for d in &dirs {
            assert!(std::path::Path::new(d).is_dir(), "{d} must exist");
        }
        let mut sorted = dirs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), dirs.len(), "no directory is listed twice");
    }

    #[test]
    fn driver_paths_outrank_distribution_paths() {
        // A host that has both must search the driver closure first, or a
        // stale system libGL shadows the one the GPU stack expects.
        let all = ["/run/opengl-driver/lib", "/usr/lib"];
        let present: Vec<&str> = all
            .into_iter()
            .filter(|d| std::path::Path::new(d).is_dir())
            .collect();
        if present.len() == 2 {
            let dirs = host_driver_paths(std::env::consts::ARCH);
            let driver = dirs.iter().position(|d| d == "/run/opengl-driver/lib");
            let usrlib = dirs.iter().position(|d| d == "/usr/lib");
            assert!(driver < usrlib);
        }
    }
}
