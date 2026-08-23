//! Host directories that supply GPU and system libraries.
//!
//! A bundled loader has its compiled-in search paths scrubbed, so anything
//! the host must still provide (libcuda, libvulkan, libGL, libva) has to be
//! named explicitly. The packed runtime and `onelf run` both need the same
//! list, and previously kept two copies in sync by comment.

use std::path::Path;

/// Host driver and system library directories for `arch`, in descending
/// priority, filtered to those that exist.
///
/// `arch` takes the `std::env::consts::ARCH` spelling of the architecture
/// the package targets. Multiarch directory names are derived from it, so a
/// package is not handed x86_64 paths on aarch64.
///
/// The well-known directories come first, then any further directory the
/// host's `ld.so.cache` names. See [`cache_dirs`] for why the cache has to be
/// read here rather than left to the loader.
pub fn host_driver_paths(arch: &str) -> Vec<String> {
    // NixOS exposes every GPU userspace driver under /run/opengl-driver,
    // populated from the active `hardware.graphics` closure. Elsewhere they
    // sit alongside the rest of the system's libraries.
    let mut dirs: Vec<&str> = vec!["/run/opengl-driver/lib", "/run/opengl-driver-32/lib"];
    dirs.extend(multiarch_dirs(arch));
    dirs.extend(["/usr/lib64", "/usr/lib", "/lib64", "/lib"]);

    let mut seen: Vec<String> = Vec::new();
    for d in dirs {
        if Path::new(d).is_dir() && !seen.iter().any(|s| s == d) {
            seen.push(d.to_string());
        }
    }
    // Appended last: these are a completion of the list above, and must not
    // displace the driver closure that has to be searched first.
    for d in cache_dirs() {
        if !seen.contains(&d) {
            seen.push(d);
        }
    }
    seen
}

/// Directories holding a library named by the host's `/etc/ld.so.cache`,
/// in the order the cache lists them, filtered to those that exist.
///
/// The bundled loader cannot consult the cache itself: `bundle-libs` blanks
/// the `/etc/` string inside it and the runtime passes `--inhibit-cache`,
/// both so a host library cannot shadow a bundled one. That leaves the fixed
/// list above as the only route to a host library, and it is a guess about
/// where a distribution puts things.
///
/// Where the guess fails, a host GPU driver loads and its own dependencies do
/// not. Gentoo slots LLVM under `/usr/lib/llvm/<n>/lib64`, so Mesa's RADV
/// driver resolves but the `libLLVM.so.<n>` behind it does not, and Vulkan
/// goes silently missing. Reading the cache here keeps the decision about
/// what the host may supply on onelf's side, while letting the answer come
/// from the host's own index instead of a hardcoded list.
fn cache_dirs() -> Vec<String> {
    let Ok(data) = std::fs::read("/etc/ld.so.cache") else {
        return Vec::new();
    };

    let mut dirs: Vec<String> = Vec::new();
    for entry in cache_entries(&data) {
        let Some((dir, _)) = entry.rsplit_once('/') else {
            continue;
        };
        if !dir.is_empty() && !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.to_string());
        }
    }
    // One stat per distinct directory rather than one per cache entry: a
    // cache routinely lists a couple of thousand libraries across a handful
    // of directories.
    dirs.retain(|d| Path::new(d).is_dir());
    dirs
}

const CACHE_MAGIC_OLD: &[u8] = b"ld.so-1.7.0";
const CACHE_MAGIC_NEW: &[u8] = b"glibc-ld.so.cache1.1";

/// Library paths recorded in a glibc loader cache image.
///
/// Two layouts exist. `ldconfig` in its compatibility mode writes the old
/// header, its entries, and then a complete new-format cache after them;
/// otherwise it writes the new format alone, which is what current
/// distributions ship. The new format is preferred wherever it appears, since
/// the old one cannot express hwcap and is only kept for compatibility.
fn cache_entries(data: &[u8]) -> Vec<&str> {
    if data.starts_with(CACHE_MAGIC_NEW) {
        return new_entries(data, 0);
    }
    if data.starts_with(CACHE_MAGIC_OLD) {
        // struct cache_file: char magic[11], then `unsigned int nlibs` at the
        // next 4-byte boundary, then 12-byte entries.
        let Some(nlibs) = read_u32(data, 12) else {
            return Vec::new();
        };
        let Some(entries_len) = (nlibs as usize).checked_mul(12) else {
            return Vec::new();
        };
        let Some(end) = entries_len.checked_add(16) else {
            return Vec::new();
        };
        // The new-format header follows, aligned to its 8-byte alignment.
        let aligned = end.next_multiple_of(8);
        if data.len() > aligned && data[aligned..].starts_with(CACHE_MAGIC_NEW) {
            return new_entries(data, aligned);
        }
        return old_entries(data, nlibs as usize, end);
    }
    Vec::new()
}

/// Entries of a new-format cache whose header starts at `base`.
fn new_entries(data: &[u8], base: usize) -> Vec<&str> {
    // Header: magic+version (20), nlibs (4), len_strings (4), flags (1),
    // padding (3), extension_offset (4), unused (12). Entries follow at 48
    // and are 24 bytes each; string offsets are relative to `base`.
    let Some(nlibs) = read_u32(data, base + 20) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..nlibs as usize {
        let Some(entry) = base.checked_add(48).and_then(|s| s.checked_add(i * 24)) else {
            break;
        };
        let Some(value) = read_u32(data, entry + 8) else {
            break;
        };
        if let Some(s) = read_str(data, base + value as usize) {
            out.push(s);
        }
    }
    out
}

/// Entries of an old-format cache with `nlibs` entries and a string table
/// beginning at `strings`.
fn old_entries(data: &[u8], nlibs: usize, strings: usize) -> Vec<&str> {
    let mut out = Vec::new();
    for i in 0..nlibs {
        let entry = 16 + i * 12;
        let Some(value) = read_u32(data, entry + 8) else {
            break;
        };
        if let Some(s) = read_str(data, strings + value as usize) {
            out.push(s);
        }
    }
    out
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// The NUL-terminated string at `at`, if it is in bounds, terminated, and
/// an absolute path. Anything else is a cache we do not understand, and a
/// relative path would resolve against the app's working directory.
fn read_str(data: &[u8], at: usize) -> Option<&str> {
    let rest = data.get(at..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    let s = std::str::from_utf8(&rest[..end]).ok()?;
    s.starts_with('/').then_some(s)
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

    /// A new-format cache image naming `paths`.
    fn new_cache(paths: &[&str]) -> Vec<u8> {
        let mut out = Vec::from(CACHE_MAGIC_NEW);
        out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // len_strings
        out.push(0); // flags
        out.extend_from_slice(&[0; 3]); // padding
        out.extend_from_slice(&0u32.to_le_bytes()); // extension_offset
        out.extend_from_slice(&[0; 12]); // unused
        assert_eq!(out.len(), 48, "entries must start at 48");

        let base = out.len();
        let strings_at = base + paths.len() * 24;
        let (offsets, strings) = string_table(paths, strings_at);
        for off in offsets {
            out.extend_from_slice(&0i32.to_le_bytes()); // flags
            out.extend_from_slice(&0u32.to_le_bytes()); // key
            out.extend_from_slice(&(off as u32).to_le_bytes()); // value
            out.extend_from_slice(&0u32.to_le_bytes()); // osversion
            out.extend_from_slice(&0u64.to_le_bytes()); // hwcap
        }
        out.extend_from_slice(&strings);
        out
    }

    /// An old-format cache image naming `paths`, optionally followed by a
    /// new-format cache naming `also`, the way glibc before 2.32 wrote it.
    ///
    /// The two formats share one string table. glibc bases old-format offsets
    /// at `&libs[nlibs]`, which is exactly where the new header is aligned to,
    /// so when both are present both formats count from the same place.
    fn old_cache(paths: &[&str], also: Option<&[&str]>) -> Vec<u8> {
        let mut out = Vec::from(CACHE_MAGIC_OLD);
        out.push(0); // pad to the 4-byte boundary nlibs sits on
        out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
        assert_eq!(out.len(), 16, "entries must start at 16");

        let entries_end = 16 + paths.len() * 12;
        let Some(also) = also else {
            let (offsets, strings) = string_table(paths, 0);
            for off in offsets {
                out.extend_from_slice(&0i32.to_le_bytes()); // flags
                out.extend_from_slice(&0u32.to_le_bytes()); // key
                out.extend_from_slice(&(off as u32).to_le_bytes()); // value
            }
            assert_eq!(out.len(), entries_end);
            out.extend_from_slice(&strings);
            return out;
        };

        let new_base = entries_end.next_multiple_of(8);
        let strings_at = new_base + 48 + also.len() * 24;
        // Both tables count from new_base, so lay the strings out once and
        // hand each format the slice of offsets that belongs to it.
        let all: Vec<&str> = paths.iter().chain(also.iter()).copied().collect();
        let (offsets, strings) = string_table(&all, strings_at - new_base);

        for off in &offsets[..paths.len()] {
            out.extend_from_slice(&0i32.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(*off as u32).to_le_bytes());
        }
        out.resize(new_base, 0);

        out.extend_from_slice(CACHE_MAGIC_NEW);
        out.extend_from_slice(&(also.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // len_strings
        out.push(0); // flags
        out.extend_from_slice(&[0; 3]); // padding
        out.extend_from_slice(&0u32.to_le_bytes()); // extension_offset
        out.extend_from_slice(&[0; 12]); // unused
        for off in &offsets[paths.len()..] {
            out.extend_from_slice(&0i32.to_le_bytes()); // flags
            out.extend_from_slice(&0u32.to_le_bytes()); // key
            out.extend_from_slice(&(*off as u32).to_le_bytes()); // value
            out.extend_from_slice(&0u32.to_le_bytes()); // osversion
            out.extend_from_slice(&0u64.to_le_bytes()); // hwcap
        }
        assert_eq!(out.len(), strings_at);
        out.extend_from_slice(&strings);
        out
    }

    fn string_table(paths: &[&str], base: usize) -> (Vec<usize>, Vec<u8>) {
        let mut offsets = Vec::new();
        let mut strings: Vec<u8> = Vec::new();
        for p in paths {
            offsets.push(base + strings.len());
            strings.extend_from_slice(p.as_bytes());
            strings.push(0);
        }
        (offsets, strings)
    }

    #[test]
    fn reads_a_new_format_cache() {
        let img = new_cache(&[
            "/usr/lib64/libc.so.6",
            "/usr/lib/llvm/22/lib64/libLLVM.so.22.1",
        ]);
        assert_eq!(
            cache_entries(&img),
            [
                "/usr/lib64/libc.so.6",
                "/usr/lib/llvm/22/lib64/libLLVM.so.22.1"
            ]
        );
    }

    #[test]
    fn prefers_the_new_cache_appended_after_an_old_one() {
        // glibc before 2.32 writes both. The old section is compatibility
        // padding; reading it instead would miss anything hwcap-tagged.
        let img = old_cache(&["/lib/old.so.1"], Some(&["/usr/lib64/new.so.2"]));
        assert_eq!(cache_entries(&img), ["/usr/lib64/new.so.2"]);
    }

    #[test]
    fn reads_an_old_format_cache_with_nothing_appended() {
        let img = old_cache(&["/lib/libz.so.1", "/usr/lib/libm.so.6"], None);
        assert_eq!(
            cache_entries(&img),
            ["/lib/libz.so.1", "/usr/lib/libm.so.6"]
        );
    }

    #[test]
    fn a_damaged_cache_yields_nothing_rather_than_panicking() {
        assert!(cache_entries(b"").is_empty());
        assert!(cache_entries(b"not a cache at all").is_empty());
        // Truncated part-way through the entry table, and a count that would
        // run far past the end of the image.
        let img = new_cache(&["/usr/lib64/libc.so.6"]);
        for cut in 0..img.len() {
            let _ = cache_entries(&img[..cut]);
        }
        let mut lying = new_cache(&["/usr/lib64/libc.so.6"]);
        lying[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(cache_entries(&lying).len() < 2);
    }

    #[test]
    fn relative_and_unterminated_entries_are_skipped() {
        // A path resolved against the app's working directory would be a way
        // in for anything that can chdir the process.
        let img = new_cache(&["not/absolute", "/usr/lib64/fine.so"]);
        assert_eq!(cache_entries(&img), ["/usr/lib64/fine.so"]);

        let mut unterminated = new_cache(&["/usr/lib64/fine.so"]);
        let last = unterminated.len() - 1;
        unterminated[last] = b'x'; // clobber the NUL
        assert!(cache_entries(&unterminated).is_empty());
    }

    #[test]
    fn cache_directories_come_after_the_well_known_ones() {
        // The driver closure and the distribution directories are ordered
        // deliberately; the cache completes that list without reordering it.
        let dirs = host_driver_paths(std::env::consts::ARCH);
        let from_cache = cache_dirs();
        let Some(first_cache_only) = dirs
            .iter()
            .position(|d| from_cache.contains(d) && !well_known(d))
        else {
            return; // this host's cache adds nothing
        };
        let last_well_known = dirs.iter().rposition(|d| well_known(d)).unwrap_or(0);
        assert!(
            last_well_known < first_cache_only,
            "cache directory ordered before a well-known one in {dirs:?}"
        );
    }

    fn well_known(dir: &str) -> bool {
        matches!(
            dir,
            "/run/opengl-driver/lib"
                | "/run/opengl-driver-32/lib"
                | "/usr/lib64"
                | "/usr/lib"
                | "/lib64"
                | "/lib"
        ) || multiarch_dirs(std::env::consts::ARCH).contains(&dir)
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
