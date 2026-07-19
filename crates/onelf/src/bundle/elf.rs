//! ELF inspection and rewriting for bundled binaries: DT_NEEDED / PT_INTERP
//! parsing, libc-family detection, RUNPATH origin rewriting, loader-path and
//! Nix-store scrubbing, and bootstrap/interp injection.

use super::*;

pub(crate) fn parse_needed(path: &Path) -> io::Result<Vec<String>> {
    parse_needed_bytes(&fs::read(path)?)
}

/// DT_NEEDED sonames from already-read ELF bytes, so a caller that also
/// needs the raw bytes (e.g. the framework string scan) reads the file once.
pub(crate) fn parse_needed_bytes(data: &[u8]) -> io::Result<Vec<String>> {
    let elf = goblin::elf::Elf::parse(data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    // nixpkgs occasionally emits DT_NEEDED entries as absolute
    // `/nix/store/<hash>/lib/libfoo.so` paths rather than plain
    // sonames. Reduce those to their basename so our resolver can
    // find the lib on the host / search paths. `strip_absolute_needed`
    // rewrites the ELF's own DT_NEEDED string after bundling so the
    // runtime loader also picks up the bundled copy via RUNPATH.
    Ok(elf
        .libraries
        .iter()
        .map(|s| {
            if s.starts_with('/') {
                Path::new(s)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(s)
                    .to_string()
            } else {
                s.to_string()
            }
        })
        .collect())
}

/// Parse PT_INTERP from an ELF binary, returning the interpreter path.
///
/// goblin returns `p_filesz - 1` bytes verbatim, so a slot padded with
/// trailing NULs (common after our in-place PT_INTERP rewrite if the
/// phdr wasn't shrunk) would leak into callers as embedded NULs in
/// the returned string. Trim them so queue lookups and `file_name()`
/// behave correctly.
pub(crate) fn parse_interp(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    let elf = goblin::elf::Elf::parse(&data).ok()?;
    elf.interpreter
        .map(|s| s.trim_end_matches('\0').to_string())
}

/// Map an ELF interpreter basename to the libc filename that serves it.
/// On musl, `ld-musl-<arch>.so.1` and `libc.musl-<arch>.so.1` are both
/// names for the same file. Returns None if no mapping is known.
pub(crate) fn libc_alias_for(interp_name: &str) -> Option<String> {
    interp_name
        .strip_prefix("ld-musl-")
        .map(|rest| format!("libc.musl-{rest}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LibcFamily {
    Musl,
    Glibc,
}

/// Detect a binary's libc family from its PT_INTERP basename.
pub(crate) fn libc_family_from_interp(interp: &str) -> Option<LibcFamily> {
    let name = Path::new(interp).file_name()?.to_str()?;
    if name.starts_with("ld-musl-") {
        Some(LibcFamily::Musl)
    } else if name.starts_with("ld-linux") {
        Some(LibcFamily::Glibc)
    } else {
        None
    }
}

/// Map a soname to the libc family it belongs to, when known.
pub(crate) fn libc_family_of_soname(soname: &str) -> Option<LibcFamily> {
    if soname == "libc.so.6" || soname.starts_with("ld-linux") {
        Some(LibcFamily::Glibc)
    } else if soname.starts_with("libc.musl-")
        || soname.starts_with("ld-musl-")
        || soname == "libc.so"
    {
        // libc.so is musl's canonical libc filename; libc.musl-*/ld-musl-* are aliases.
        Some(LibcFamily::Musl)
    } else {
        None
    }
}

/// Parse RPATH and RUNPATH entries from an ELF binary.
/// Expand a single runpath entry, resolving `$ORIGIN` / `${ORIGIN}` to the
/// ELF's parent directory (the exact relative shape Nix and vendor binaries
/// emit). Non-`$ORIGIN` entries pass through unchanged.
pub(crate) fn expand_runpath_entry(entry: &str, origin: &Path) -> PathBuf {
    if let Some(rest) = entry.strip_prefix("${ORIGIN}") {
        origin.join(rest.trim_start_matches('/'))
    } else if let Some(rest) = entry.strip_prefix("$ORIGIN") {
        origin.join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(entry)
    }
}

/// Split colon-separated runpath strings into their component directories,
/// expand `$ORIGIN` relative to `origin`, and keep only those that exist.
pub(crate) fn resolve_runpath_dirs<'a>(
    raw: impl Iterator<Item = &'a str>,
    origin: &Path,
) -> Vec<PathBuf> {
    raw.flat_map(|s| s.split(':'))
        .filter(|s| !s.is_empty())
        .map(|entry| expand_runpath_entry(entry, origin))
        .filter(|p| p.is_dir())
        .collect()
}

pub(crate) fn parse_rpaths(path: &Path) -> Vec<PathBuf> {
    let Ok(data) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(elf) = goblin::elf::Elf::parse(&data) else {
        return Vec::new();
    };
    // The ELF's own directory, for `$ORIGIN` expansion.
    let origin = path.parent().unwrap_or_else(|| Path::new("."));
    // The dynamic loader ignores DT_RPATH when DT_RUNPATH is present, so
    // match that precedence rather than chaining both (which could pull a
    // library from a dir the loader would never search).
    let raw = if !elf.runpaths.is_empty() {
        &elf.runpaths
    } else {
        &elf.rpaths
    };
    resolve_runpath_dirs(raw.iter().copied(), origin)
}

/// Rewrite RPATH/RUNPATH to `$ORIGIN/../lib` so the bundled ELF finds its
/// transitive libraries via its own on-disk location, never via
/// `LD_LIBRARY_PATH`. That matters because `LD_LIBRARY_PATH` is a
/// per-process env variable that gets inherited into host binaries the
/// app may spawn (for example, `postgres` uses `popen(3)` which execs
/// `/bin/sh` - a host binary linked against the host's glibc). If we
/// left our bundle dir on `LD_LIBRARY_PATH`, the host shell would load
/// our newer `libc.so.6` against its own older `ld-linux.so.2` and
/// crash with a null deref in the loader. Using `$ORIGIN/../lib` keeps
/// the bundle's library search scoped to the bundled ELF itself.
///
/// First tries in-place patching of an existing DT_RPATH/DT_RUNPATH
/// slot. If the binary has no slot or it's too small for our string
/// (e.g. Bun, Go, Zig outputs), falls back to `patchelf --set-rpath`
/// when available.
///
/// The outcome matters for re-exec safety: an executable that ends up
/// without a baked-in `$ORIGIN` RUNPATH can only find its bundled libs
/// via `LD_LIBRARY_PATH`, which is wiped when the app re-execs itself in
/// a sandbox (`clearenv()` + `execve`). The caller surfaces those so the
/// package isn't silently fragile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunpathOutcome {
    /// `$ORIGIN` RUNPATH is baked into the ELF (in-place or via patchelf).
    Set,
    /// No RUNPATH needed: static binary, bare lib, or no DT_NEEDED.
    NotNeeded,
    /// Executable with deps but RUNPATH could not be guaranteed (no
    /// in-place slot and patchelf missing or failed). Relies on
    /// `LD_LIBRARY_PATH`; not sandbox-re-exec-safe.
    Unguaranteed,
    /// Executable with a self-extract trailer: patchelf would clobber
    /// the trailer, so RUNPATH can't be added. Known limitation.
    SelfExtract,
}

pub(crate) fn set_origin_runpath(path: &Path) -> io::Result<RunpathOutcome> {
    // Cover binaries at depth 1 (e.g. bin/foo), 2 (libexec/podman/x),
    // and 3 (share/pkg/helpers/y). Nonexistent entries are silently
    // ignored by the dynamic loader, so this is safe to apply
    // uniformly without knowing where each ELF sits.
    const NEW: &str = "$ORIGIN/../lib:$ORIGIN/../../lib:$ORIGIN/../../../lib";
    let new_bytes = NEW.as_bytes();
    let data = fs::read(path)?;

    // Self-extracting binaries (e.g. pre-1.3.12 Bun) have a trailer at
    // the end of the file. patchelf can grow the file when adding a
    // missing DT_RUNPATH, which would invalidate the trailer. The
    // in-place rewrite is safe (same file size), so we still attempt
    // that, but we skip the patchelf fallback for these binaries.
    let is_self_extract = has_self_extract_trailer(&data);

    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    // Only meaningful for binaries with dynamic dependencies. A bottom-
    // of-stack lib like libc.so.6 or the dynamic loader has no DT_NEEDED
    // entries and doesn't need DT_RUNPATH itself.
    let has_needed = !elf.libraries.is_empty();
    // PT_INTERP marks an executable; pure shared libraries lack it. We
    // only warn / fall back to patchelf for executables, since libs
    // typically resolve their deps via the executable's DT_RUNPATH.
    // glibc's libc.so.6 (and ld.so) carry PT_INTERP but are libraries,
    // distinguished by a DT_SONAME; exclude anything with a SONAME so
    // they aren't mis-flagged as un-RUNPATH'd app executables.
    let has_soname = elf.soname.is_some();
    let is_executable = !has_soname
        && elf
            .program_headers
            .iter()
            .any(|p| p.p_type == goblin::elf::program_header::PT_INTERP);

    // Find .dynstr section file offset
    let dynstr_offset = elf
        .section_headers
        .iter()
        .find(|sh| elf.shdr_strtab.get_at(sh.sh_name) == Some(".dynstr"))
        .map(|sh| sh.sh_offset as usize);

    let dynamic_present = elf.dynamic.is_some();
    // Track per-slot outcome: the in-place rewrite is trustworthy only when
    // *every* present DT_RPATH/DT_RUNPATH slot was rewritten. Rewriting some
    // and skipping a too-small one would leave a stale runpath behind, so in
    // that case we fall through to patchelf, which resets the runpath whole.
    let mut slots_total = 0usize;
    let mut slots_rewritten = 0usize;

    if let (Some(dynstr_offset), Some(dynamic)) = (dynstr_offset, &elf.dynamic) {
        let mut modified = data.clone();
        for dyn_entry in &dynamic.dyns {
            if dyn_entry.d_tag == goblin::elf::dynamic::DT_RPATH
                || dyn_entry.d_tag == goblin::elf::dynamic::DT_RUNPATH
            {
                slots_total += 1;
                let file_pos = dynstr_offset + dyn_entry.d_val as usize;
                if file_pos >= modified.len() {
                    continue;
                }
                let mut end = file_pos;
                while end < modified.len() && modified[end] != 0 {
                    end += 1;
                }
                while end < modified.len() && modified[end] == 0 {
                    end += 1;
                }
                let slot_size = end - file_pos;
                if new_bytes.len() + 1 > slot_size {
                    // Slot too small; will fall back to patchelf below.
                    continue;
                }
                modified[file_pos..file_pos + new_bytes.len()].copy_from_slice(new_bytes);
                for i in new_bytes.len()..slot_size {
                    modified[file_pos + i] = 0;
                }
                slots_rewritten += 1;
            }
        }
        if slots_total > 0 && slots_rewritten == slots_total {
            fs::write(path, &modified)?;
            return Ok(RunpathOutcome::Set);
        }
    }

    drop(elf);

    if !dynamic_present || !has_needed {
        // Nothing depends on libs (static binary, libc.so itself, the
        // dynamic loader, etc.). DT_RUNPATH wouldn't help here.
        return Ok(RunpathOutcome::NotNeeded);
    }

    if !is_executable {
        // Shared libraries usually resolve their deps via the
        // executable's DT_RUNPATH (transitive search). Skip patchelf
        // and the noisy warning for bare libs.
        return Ok(RunpathOutcome::NotNeeded);
    }

    if is_self_extract {
        // Don't risk patchelf growing the file and clobbering the
        // self-extract trailer. The runtime still sets LD_LIBRARY_PATH
        // as a fallback for these binaries.
        return Ok(RunpathOutcome::SelfExtract);
    }

    // No usable in-place slot. Fall back to patchelf, which can
    // either resize an existing slot or add a fresh DT_RUNPATH by
    // growing the file's string table.
    if let Some(patchelf) = which_patchelf() {
        let status = std::process::Command::new(&patchelf)
            .arg("--force-rpath")
            .arg("--set-rpath")
            .arg(NEW)
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        match status {
            Ok(o) if o.status.success() => return Ok(RunpathOutcome::Set),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                eprintln!(
                    "  {} patchelf failed for {}: {}",
                    color::bold_red("warning:"),
                    path.display(),
                    stderr.trim()
                );
            }
            Err(e) => {
                eprintln!(
                    "  {} could not run patchelf for {}: {e}",
                    color::bold_red("warning:"),
                    path.display(),
                );
            }
        }
    }
    // No patchelf available and no in-place slot. The runtime still sets
    // LD_LIBRARY_PATH as a fallback for the initial launch, but it won't
    // survive a sandboxed re-exec. The caller reports this.
    Ok(RunpathOutcome::Unguaranteed)
}

/// Finalize every ELF in `directory` for portability: rewrite RUNPATH to
/// `$ORIGIN/../lib`, scrub `/nix/store` paths, and strip absolute DT_NEEDED
/// entries, temporarily granting owner-write to read-only files. Returns
/// `(rewritten, scrubbed, unguaranteed, self_extract)`.
pub(crate) fn finalize_tree(directory: &Path) -> (usize, usize, Vec<PathBuf>, Vec<PathBuf>) {
    let mut rewritten = 0usize;
    let mut scrubbed = 0usize;
    let mut unguaranteed: Vec<PathBuf> = Vec::new();
    let mut self_extract: Vec<PathBuf> = Vec::new();
    for path in find_elf_files(directory) {
        let perms = fs::metadata(&path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o755);
        let needs_chmod = perms & 0o200 == 0;
        if needs_chmod {
            let _ = fs::set_permissions(&path, PermissionsExt::from_mode(perms | 0o200));
        }
        tally_origin_runpath(&path, &mut rewritten, &mut unguaranteed, &mut self_extract);
        let before = fs::metadata(&path).and_then(|m| m.modified()).ok();
        let _ = scrub_nix_store_paths(&path);
        let _ = strip_absolute_needed(&path);
        let after = fs::metadata(&path).and_then(|m| m.modified()).ok();
        if before.is_some() && before != after {
            scrubbed += 1;
        }
        // Re-pin the mtime: the RUNPATH/scrub/strip writes above stamped
        // "now", undoing the copy step's normalization. Without this a
        // second bundle-libs run yields a metadata-different tree.
        normalize_mtime(&path);
        if needs_chmod {
            let _ = fs::set_permissions(&path, PermissionsExt::from_mode(perms));
        }
    }
    (rewritten, scrubbed, unguaranteed, self_extract)
}

/// Apply `set_origin_runpath` and fold the outcome into the running
/// tallies. `set` counts binaries that got a baked-in RUNPATH;
/// `unguaranteed` / `self_extract` collect executables that did not, so
/// the caller can warn that they aren't sandbox-re-exec-safe.
pub(crate) fn tally_origin_runpath(
    path: &Path,
    set: &mut usize,
    unguaranteed: &mut Vec<PathBuf>,
    self_extract: &mut Vec<PathBuf>,
) {
    match set_origin_runpath(path) {
        Ok(RunpathOutcome::Set) => *set += 1,
        Ok(RunpathOutcome::Unguaranteed) => unguaranteed.push(path.to_path_buf()),
        Ok(RunpathOutcome::SelfExtract) => self_extract.push(path.to_path_buf()),
        Ok(RunpathOutcome::NotNeeded) | Err(_) => {}
    }
}

/// Warn that the listed executables could not get a baked-in `$ORIGIN`
/// RUNPATH and therefore won't survive a sandboxed re-exec. Printed once
/// per bundling pass; empty input prints nothing.
pub(crate) fn report_unguaranteed_runpath(unguaranteed: &[PathBuf], self_extract: &[PathBuf]) {
    if !unguaranteed.is_empty() {
        eprintln!(
            "{} {} executable(s) have no baked-in $ORIGIN RUNPATH and rely \
             on LD_LIBRARY_PATH:",
            color::bold_red("warning:"),
            unguaranteed.len()
        );
        for p in unguaranteed {
            eprintln!("  - {}", p.display());
        }
        eprintln!(
            "  These break if the app re-execs itself in a sandbox \
             (clearenv). Install `patchelf` (or set ONELF_PATCHELF) and \
             repack to make them re-exec-safe."
        );
    }
    if !self_extract.is_empty() {
        eprintln!(
            "{} {} self-extracting executable(s) can't take a baked-in \
             RUNPATH (would clobber the embedded payload):",
            color::bold_red("warning:"),
            self_extract.len()
        );
        for p in self_extract {
            eprintln!("  - {}", p.display());
        }
        eprintln!(
            "  These rely on the runtime's LD_LIBRARY_PATH and are not \
             sandbox-re-exec-safe."
        );
    }
}

/// Detect binaries that embed a self-extracting payload at the end of
/// the file. Bootstrap injection appends to the file, which would clobber
/// such payloads and prevent runtime detection.
///
/// Currently detects: pre-1.3.12 Bun (`bun build --compile`) binaries
/// which end with `\n---- Bun! ----\n` followed by an 8-byte length.
pub(crate) fn has_self_extract_trailer(data: &[u8]) -> bool {
    // Bun's trailer is 16 bytes; pre-1.3.12 also has an 8-byte length
    // word after it (so check at offsets -16 and -24). Modern Bun
    // (>=1.3.12) uses a `.bun` ELF section instead and is unaffected.
    const BUN_TRAILER: &[u8] = b"\n---- Bun! ----\n";
    if data.len() >= BUN_TRAILER.len() && data.ends_with(BUN_TRAILER) {
        return true;
    }
    if data.len() >= BUN_TRAILER.len() + 8
        && &data[data.len() - BUN_TRAILER.len() - 8..data.len() - 8] == BUN_TRAILER
    {
        return true;
    }
    false
}

/// Locate patchelf in PATH (or ONELF_PATCHELF override).
pub(crate) fn which_patchelf() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ONELF_PATCHELF") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let p = PathBuf::from(dir).join("patchelf");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Rewrite any absolute-path DT_NEEDED entry to just its basename. The
/// pack host's nixpkgs stack sometimes emits a full
/// `/nix/store/<hash>-name/lib/libfoo.so` as the DT_NEEDED string. The
/// dynamic loader treats those literally and ignores `RUNPATH` /
/// `LD_LIBRARY_PATH`, so a binary built with them will try to `open`
/// that exact path on the user's machine and fail. Stripping to the
/// basename puts the lookup back on the standard search path and
/// picks up our bundled copy via `$ORIGIN/../lib`.
///
/// Operates in place: writes the new basename over the old string and
/// NUL-pads the rest of the slot. The old slot is always longer than
/// the new basename, so this never needs to grow the string table.
pub(crate) fn strip_absolute_needed(path: &Path) -> io::Result<()> {
    let data = fs::read(path)?;
    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let dynstr_offset = elf
        .section_headers
        .iter()
        .find(|sh| elf.shdr_strtab.get_at(sh.sh_name) == Some(".dynstr"))
        .map(|sh| sh.sh_offset as usize);
    let Some(dynstr_offset) = dynstr_offset else {
        return Ok(());
    };
    let Some(dynamic) = &elf.dynamic else {
        return Ok(());
    };

    let mut modified = data;
    let mut changed = false;

    for dyn_entry in &dynamic.dyns {
        if dyn_entry.d_tag != goblin::elf::dynamic::DT_NEEDED {
            continue;
        }
        let file_pos = dynstr_offset + dyn_entry.d_val as usize;
        if file_pos >= modified.len() || modified[file_pos] != b'/' {
            continue;
        }
        // Read the current (absolute) path from the string table.
        let mut end = file_pos;
        while end < modified.len() && modified[end] != 0 {
            end += 1;
        }
        let original = &modified[file_pos..end];
        let slot_size = end - file_pos;

        let basename_start = match original.iter().rposition(|&b| b == b'/') {
            Some(p) => p + 1,
            None => 0,
        };
        let basename_len = original.len() - basename_start;
        if basename_len == 0 || basename_len >= slot_size {
            continue;
        }

        let basename: Vec<u8> = original[basename_start..].to_vec();
        modified[file_pos..file_pos + basename_len].copy_from_slice(&basename);
        for i in basename_len..slot_size {
            modified[file_pos + i] = 0;
        }
        changed = true;
    }

    if changed {
        fs::write(path, &modified)?;
    }
    Ok(())
}

pub(crate) fn is_excluded(soname: &str, excludes: &[&str]) -> bool {
    excludes.iter().any(|pat| soname.starts_with(pat))
}

/// True for sonames that denote the dynamic linker itself.
pub(crate) fn is_dynamic_loader(soname: &str) -> bool {
    soname.starts_with("ld-linux") || soname.starts_with("ld-musl-") || soname == "ld.so"
}

/// Rewrite absolute-path byte sequences baked into the dynamic loader.
///
/// glibc's `ld-linux` hardcodes its build-time `/etc/ld.so.cache`,
/// `/etc/ld-nix.so.preload`, `/nix/store/<hash>-glibc-X/lib/`, and a
/// few other absolute paths. Those exist on the packer's machine but
/// not on the user's; worse, if any do exist, they'll point at a
/// libc that disagrees with the one we bundled. The fix is to replace
/// each prefix with a path that is guaranteed not to resolve (starts
/// with `/XXX`), keeping byte length identical so ELF offsets stay
/// valid.
///
/// This is the same idea as sharun's `sed` pass, done in pure Rust
/// with a more targeted prefix list.
pub(crate) fn scrub_loader_paths(path: &Path) -> io::Result<()> {
    let mut data = fs::read(path)?;
    let mut changed = false;
    // Each pattern and replacement are equal length to avoid any ELF
    // structure shifts. Replacements are paths that simply don't exist
    // on any sane system.
    let replacements: &[(&[u8], &[u8])] = &[
        (b"/etc/", b"/XXX/"),
        (b"/usr/", b"/XXX/"),
        (b"/nix/", b"/XXX/"),
        // /lib/ and /lib64/ appear as glibc's hardcoded fallback
        // library search paths. Our bundled libs live in `lib/` (no
        // leading slash), so scrubbing absolute /lib doesn't hurt.
        (b"/lib/", b"/XXX/"),
        (b"/lib64/", b"/XXX///"),
    ];

    for (needle, replace) in replacements {
        debug_assert_eq!(needle.len(), replace.len());
        let len = needle.len();
        let mut i = 0;
        while i + len <= data.len() {
            if &data[i..i + len] == *needle {
                data[i..i + len].copy_from_slice(replace);
                changed = true;
                i += len;
            } else {
                i += 1;
            }
        }
    }

    if changed {
        fs::write(path, &data)?;
    }
    Ok(())
}

/// Rewrite specific `/nix/store/<hash>-<name>-<version>/...` strings
/// baked into a bundled ELF with sensible host equivalents. Called for
/// every bundled non-loader ELF, not just the loader.
///
/// nixpkgs typically compiles postgres with `--with-system-tzdata=<store>`
/// and embeds the full path to the `locale` binary it will shell out
/// to. Both paths exist only on the packer's machine. On the user's
/// machine postgres prints a parade of warnings about the missing
/// directory, then falls back to internal UTC-only behavior and
/// still-functional locale defaults. The bundle still works, but the
/// noise is confusing.
///
/// Replacements are equal-length to avoid any ELF structure shifts,
/// with the replacement null-padded to the original slot size. We
/// target the suffix (e.g. `/share/zoneinfo`, `/bin/locale`) and walk
/// back to the nearest NUL to find the start of the whole path
/// string.
pub(crate) fn scrub_nix_store_paths(path: &Path) -> io::Result<()> {
    let mut data = fs::read(path)?;
    let mut changed = false;

    // (suffix to find, replacement path, friendly name)
    let rewrites: &[(&[u8], &[u8])] = &[
        (b"/share/zoneinfo", b"/usr/share/zoneinfo"),
        (b"/bin/locale", b"/usr/bin/locale"),
    ];

    for (suffix, replacement) in rewrites {
        let mut i = 0;
        while i + suffix.len() <= data.len() {
            if &data[i..i + suffix.len()] != *suffix {
                i += 1;
                continue;
            }
            // Walk back to find the start of this C string.
            let mut start = i;
            while start > 0 && data[start - 1] != 0 {
                start -= 1;
            }
            // Only touch strings rooted in /nix/store/.
            if start + 11 > data.len() || &data[start..start + 11] != b"/nix/store/" {
                i = i + suffix.len();
                continue;
            }
            // Find end of string: walk forward to the NUL.
            let mut end = i + suffix.len();
            while end < data.len() && data[end] != 0 {
                end += 1;
            }
            let slot = end - start;
            if replacement.len() + 1 > slot {
                // Shouldn't happen for these specific replacements,
                // but guard just in case.
                i = end;
                continue;
            }
            data[start..start + replacement.len()].copy_from_slice(replacement);
            for b in &mut data[start + replacement.len()..end] {
                *b = 0;
            }
            changed = true;
            i = end;
        }
    }

    if changed {
        fs::write(path, &data)?;
    }
    Ok(())
}

/// Inject the AT_EXECFN bootstrap into a single ELF binary.
///
/// Repurposes PT_INTERP as PT_LOAD containing the bootstrap payload +
/// metadata. At runtime the bootstrap reads AT_EXECFN from the aux
/// vector, computes the interpreter path relative to the binary's own
/// location (not CWD), mmaps the interpreter, and jumps to its entry.
///
/// Page alignment for the injected bootstrap `PT_LOAD` segment. aarch64
/// kernels may use a 4K, 16K, or 64K page size and the target host can
/// differ from the build host, so `0x10000` is used there (valid under every
/// aarch64 page size); x86-64 keeps 4K.
pub(crate) fn bootstrap_page_align(is_aarch64: bool) -> u64 {
    if is_aarch64 { 0x10000 } else { 0x1000 }
}

/// Returns Ok(true) if injected, Ok(false) if the binary has no
/// PT_INTERP (static, shared lib, or already injected).
pub(crate) fn inject_relative_interp(path: &Path, rel_interp: &str) -> io::Result<bool> {
    use crate::payload;
    use goblin::elf::program_header::PT_INTERP;

    let data = fs::read(path)?;

    // Skip self-extracting binaries that store metadata at file end.
    // Appending the bootstrap PT_LOAD here would clobber their trailer
    // and break payload detection (e.g. pre-1.3.12 Bun-compiled
    // binaries). For these, the runtime sets LD_LIBRARY_PATH and the
    // kernel exec resolves PT_INTERP normally.
    if has_self_extract_trailer(&data) {
        eprintln!(
            "  note: {} appears to be a self-extracting binary \
             (Bun-compiled or similar); skipping bootstrap injection",
            path.display(),
        );
        return Ok(false);
    }

    let elf = goblin::elf::Elf::parse(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    if elf.header.e_ident[4] != 2 || elf.header.e_ident[5] != 1 {
        return Ok(false); // 64-bit little-endian only
    }
    let is_x86_64 = elf.header.e_machine == goblin::elf::header::EM_X86_64;
    let is_aarch64 = elf.header.e_machine == goblin::elf::header::EM_AARCH64;
    if !is_x86_64 && !is_aarch64 {
        return Ok(false);
    }

    let phdr_idx = match elf
        .program_headers
        .iter()
        .position(|p| p.p_type == PT_INTERP)
    {
        Some(i) => i,
        None => return Ok(false),
    };

    let highest_vend: u64 = elf
        .program_headers
        .iter()
        .filter(|p| p.p_type == goblin::elf::program_header::PT_LOAD)
        .map(|p| p.p_vaddr + p.p_memsz)
        .max()
        .unwrap_or(0);

    // The same value drives new_vaddr, the file-offset padding, and p_align,
    // so the kernel's `p_offset % p_align == p_vaddr % p_align` rule holds.
    let page_size: u64 = bootstrap_page_align(is_aarch64);
    let new_vaddr = (highest_vend + page_size - 1) & !(page_size - 1);
    let orig_entry = elf.header.e_entry;
    let e_phoff = elf.header.e_phoff as usize;
    let e_phentsize = elf.header.e_phentsize as usize;
    drop(elf);

    let e_machine = if is_x86_64 {
        goblin::elf::header::EM_X86_64
    } else {
        goblin::elf::header::EM_AARCH64
    };
    let Some(code) = payload::bootstrap_blob(e_machine) else {
        eprintln!(
            "  {} onelf was built without the bootstrap payload for this target's \
             architecture; skipping relative-interp injection for {}",
            color::bold_red("warning:"),
            path.display()
        );
        return Ok(false);
    };
    let rel_bytes = rel_interp.as_bytes();

    // Build: [code] [padding to 8-byte align] [orig_entry u64] [path_len u16] [path NUL]
    let mut blob = Vec::with_capacity(code.len() + 64);
    blob.extend_from_slice(code);
    while blob.len() % 8 != 0 {
        blob.push(0);
    }
    let metadata_offset = blob.len();
    let entry_delta = (orig_entry as i64) - (new_vaddr as i64);
    blob.extend_from_slice(&entry_delta.to_le_bytes());
    blob.extend_from_slice(&(rel_bytes.len() as u16).to_le_bytes());
    blob.extend_from_slice(rel_bytes);
    blob.push(0);

    // Patch the trampoline's metadata-pointer instruction.
    if is_x86_64 {
        let disp = (metadata_offset as i32) - (payload::X86_64_METADATA_LEA_RIP as i32);
        blob[payload::X86_64_METADATA_LEA_DISP_OFFSET
            ..payload::X86_64_METADATA_LEA_DISP_OFFSET + 4]
            .copy_from_slice(&disp.to_le_bytes());
    } else {
        payload::patch_aarch64_adr(&mut blob, metadata_offset);
    }

    let mut modified = data;
    // Pad to page alignment so p_offset % p_align == p_vaddr % p_align.
    // The kernel rejects PT_LOAD segments where this doesn't hold.
    let page = page_size as usize;
    while modified.len() % page != 0 {
        modified.push(0);
    }
    let file_offset = modified.len() as u64;
    let blob_len = blob.len() as u64;
    modified.extend_from_slice(&blob);

    // Overwrite PT_INTERP phdr -> PT_LOAD, then swap it to the end of
    // the phdr table. The bootstrap has the highest vaddr and the kernel
    // uses the FIRST PT_LOAD to compute the ASLR base. If our high-vaddr
    // segment is first, the base is too high and original segments at
    // lower vaddrs fall outside the reserved region.
    let phdr_off = e_phoff + phdr_idx * e_phentsize;
    modified[phdr_off..phdr_off + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    modified[phdr_off + 4..phdr_off + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R|PF_X
    modified[phdr_off + 8..phdr_off + 16].copy_from_slice(&file_offset.to_le_bytes());
    modified[phdr_off + 16..phdr_off + 24].copy_from_slice(&new_vaddr.to_le_bytes());
    modified[phdr_off + 24..phdr_off + 32].copy_from_slice(&new_vaddr.to_le_bytes());
    modified[phdr_off + 32..phdr_off + 40].copy_from_slice(&blob_len.to_le_bytes());
    modified[phdr_off + 40..phdr_off + 48].copy_from_slice(&blob_len.to_le_bytes());
    modified[phdr_off + 48..phdr_off + 56].copy_from_slice(&page_size.to_le_bytes());

    // Swap our phdr entry with the last one so original PT_LOADs come first.
    let e_phnum = u16::from_le_bytes(modified[56..58].try_into().unwrap()) as usize;
    let last_phdr_off = e_phoff + (e_phnum - 1) * e_phentsize;
    if phdr_off != last_phdr_off {
        let mut tmp = vec![0u8; e_phentsize];
        tmp.copy_from_slice(&modified[phdr_off..phdr_off + e_phentsize]);
        modified.copy_within(last_phdr_off..last_phdr_off + e_phentsize, phdr_off);
        modified[last_phdr_off..last_phdr_off + e_phentsize].copy_from_slice(&tmp);
    }

    // Rewrite e_entry
    modified[24..32].copy_from_slice(&new_vaddr.to_le_bytes());

    fs::write(path, &modified)?;
    Ok(true)
}

/// Outcome of trying to make an entrypoint load the onelf-env
/// constructor (re-exec-safe `.onelf/env` / `.onelf/preload`).
pub(crate) enum EnvNeededOutcome {
    /// `libonelf-env.so` is now a DT_NEEDED of the binary.
    Added,
    /// Already a DT_NEEDED (idempotent repack).
    AlreadyPresent,
    /// No onelf-env blob built for this arch; runtime-only env.
    NoBlobForArch,
    /// patchelf unavailable, so DT_NEEDED couldn't be added; the binary
    /// falls back to runtime-set env (not sandbox-re-exec-safe).
    NoPatchelf,
    /// Self-extract trailer or unsupported ELF: left untouched.
    Skipped,
}

/// Stage the arch-appropriate `libonelf-env.so` into `lib_dest` and add
/// it as a `DT_NEEDED` of `path`. Run on the pristine binary *before*
/// bootstrap injection so patchelf operates on a normal ELF (the
/// bootstrap later only repurposes PT_INTERP and appends at EOF, which
/// doesn't disturb the added DT_NEEDED).
pub(crate) fn add_onelf_env_needed(path: &Path, lib_dest: &Path) -> io::Result<EnvNeededOutcome> {
    let data = fs::read(path)?;
    if data.len() < 20 || &data[0..4] != b"\x7fELF" || data[4] != 2 {
        return Ok(EnvNeededOutcome::Skipped); // not a 64-bit ELF
    }
    // patchelf would grow a self-extract binary and clobber its trailer.
    if has_self_extract_trailer(&data) {
        return Ok(EnvNeededOutcome::Skipped);
    }
    let e_machine = u16::from_le_bytes([data[18], data[19]]);
    let Some(blob) = crate::payload::onelf_env_blob(e_machine) else {
        return Ok(EnvNeededOutcome::NoBlobForArch);
    };

    // Skip if this binary already lists the constructor (idempotent).
    if let Ok(elf) = goblin::elf::Elf::parse(&data) {
        if elf
            .libraries
            .iter()
            .any(|l| *l == crate::payload::ONELF_ENV_SONAME)
        {
            return Ok(EnvNeededOutcome::AlreadyPresent);
        }
    }

    // Stage the blob into lib/ (write once; idempotent across binaries).
    let dest = lib_dest.join(crate::payload::ONELF_ENV_SONAME);
    let need_write = match fs::read(&dest) {
        Ok(existing) => existing != blob,
        Err(_) => true,
    };
    if need_write {
        fs::create_dir_all(lib_dest)?;
        fs::write(&dest, blob)?;
        let _ = fs::set_permissions(&dest, std::os::unix::fs::PermissionsExt::from_mode(0o755));
        normalize_mtime(&dest);
    }

    let Some(patchelf) = which_patchelf() else {
        return Ok(EnvNeededOutcome::NoPatchelf);
    };
    let out = std::process::Command::new(&patchelf)
        .arg("--add-needed")
        .arg(crate::payload::ONELF_ENV_SONAME)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(EnvNeededOutcome::Added),
        Ok(o) => Err(io::Error::other(
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        )),
        Err(e) => Err(e),
    }
}

/// Walk every ELF under `app_dir` and inject the AT_EXECFN bootstrap
/// so the bundled interpreter is found relative to each binary's own
/// location. CWD-independent. Returns the count of injected files.
pub(crate) fn inject_bootstraps(app_dir: &Path, lib_dest: &Path) -> io::Result<usize> {
    let rel_lib = lib_dest
        .strip_prefix(app_dir)
        .unwrap_or(lib_dest)
        .to_path_buf();

    let mut injected = 0usize;
    let mut env_added = 0usize;
    let mut env_no_patchelf: Vec<PathBuf> = Vec::new();
    let mut env_no_blob = false;
    for path in find_elf_files(app_dir) {
        let Some(interp) = parse_interp(&path) else {
            continue;
        };
        let Some(basename) = Path::new(&interp).file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let bundled = lib_dest.join(basename);
        if !bundled.exists() {
            continue;
        }
        // Skip everything in lib/ (shared libs, the ld, libc, etc).
        // Only inject into application binaries outside the lib dir.
        if path.starts_with(lib_dest) {
            continue;
        }

        // Compute relative path from binary's dir to the bundled loader.
        let rel_bin = match path.strip_prefix(app_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let depth = rel_bin
            .parent()
            .map(|p| p.components().count())
            .unwrap_or(0);
        let mut rel = PathBuf::new();
        for _ in 0..depth {
            rel.push("..");
        }
        rel.push(&rel_lib);
        rel.push(basename);
        let rel_interp = rel.to_string_lossy().into_owned();

        let perms = fs::metadata(&path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o755);
        let needs_chmod = perms & 0o200 == 0;
        if needs_chmod {
            let _ = fs::set_permissions(
                &path,
                std::os::unix::fs::PermissionsExt::from_mode(perms | 0o200),
            );
        }
        // Make .onelf/env + .onelf/preload re-exec-safe by injecting the
        // onelf-env constructor as a DT_NEEDED (resolved via the
        // $ORIGIN RUNPATH set earlier). Done before bootstrap injection
        // so patchelf sees a normal ELF.
        match add_onelf_env_needed(&path, lib_dest) {
            Ok(EnvNeededOutcome::Added) => env_added += 1,
            Ok(EnvNeededOutcome::AlreadyPresent) => {}
            Ok(EnvNeededOutcome::NoBlobForArch) => env_no_blob = true,
            Ok(EnvNeededOutcome::NoPatchelf) => env_no_patchelf.push(path.clone()),
            Ok(EnvNeededOutcome::Skipped) => {}
            Err(e) => {
                eprintln!(
                    "  {} could not add onelf-env to {}: {e}",
                    color::bold_red("warning:"),
                    path.display()
                );
            }
        }

        match inject_relative_interp(&path, &rel_interp) {
            Ok(true) => injected += 1,
            Ok(false) => {}
            Err(e) => {
                eprintln!(
                    "  {} could not inject bootstrap into {}: {e}",
                    color::bold_red("warning:"),
                    path.display()
                );
            }
        }
        // Re-pin the mtime: the env-needed and interp injections above run
        // after finalize_tree normalized it, so without this the target
        // executable carries a wall-clock mtime and breaks reproducibility.
        normalize_mtime(&path);
        if needs_chmod {
            let _ = fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(perms));
        }
    }

    if env_added > 0 {
        eprintln!(
            "{} onelf-env (re-exec-safe .onelf/env) into {} binaries",
            color::bold("Injected"),
            env_added
        );
    }
    if !env_no_patchelf.is_empty() {
        eprintln!(
            "{} patchelf unavailable; {} executable(s) won't re-apply \
             .onelf/env after a sandboxed re-exec:",
            color::bold_red("warning:"),
            env_no_patchelf.len()
        );
        for p in &env_no_patchelf {
            eprintln!("  - {}", p.display());
        }
        eprintln!(
            "  Install `patchelf` (or set ONELF_PATCHELF) and repack for \
             re-exec-safe env."
        );
    }
    if env_no_blob {
        eprintln!(
            "{} no onelf-env blob built for this target arch; \
             .onelf/env is runtime-only (not sandbox-re-exec-safe). \
             Build it via crates/onelf/src/payload/Makefile.",
            color::bold_red("warning:"),
        );
    }
    Ok(injected)
}
