# Execution Modes

When you run a packed file, the runtime chooses one of four modes based on
what the system supports and how the package was configured. You can force
a specific one with `ONELF_MODE`:

```bash
ONELF_MODE=fuse    ./myapp.onelf
ONELF_MODE=tmpfs   ./myapp.onelf
ONELF_MODE=memfd   ./myapp.onelf
ONELF_MODE=cache   ./myapp.onelf
```

The default is "try them in order, fall back on failure".

## Mode 1: memfd (fastest, most limited)

When available, the package is decompressed straight into an anonymous
`memfd` file descriptor and executed via `/proc/self/fd/N`. Nothing
touches the filesystem. No mount, no extraction, no temp dir.

**Requirements:**
- The entrypoint is a single self-contained binary (no bundled `lib/`).
- The binary is truly static, or the `[package] memfd = true` flag was
  set explicitly at pack time.

onelf auto-detects static binaries (no `DT_NEEDED`) and marks them
memfd-eligible. You'll see `[memfd]` next to the entrypoint in `onelf
info`.

## Mode 2: FUSE via user namespace (default for most packages)

The runtime unshares a user+mount namespace, opens `/dev/fuse` directly,
and mounts the package as a private read-only filesystem. The entrypoint
is `exec`'d from the mount.

**Why this is great:**
- No external helpers. No `fusermount3` in PATH required.
- The mount is **invisible** to other processes on the host. Check
  `mount | grep onelf` and you won't see anything.
- When the last process in the user namespace exits, the kernel tears
  the mount down automatically. Zero cleanup code.
- On-demand block decompression. The runtime only decompresses the
  blocks it reads, so startup is fast even for huge packages.

**Requirements:**
- Kernel with FUSE support (essentially every modern Linux).
- Unprivileged user namespaces enabled
  (`/proc/sys/kernel/unprivileged_userns_clone` is `1` or missing).

## Mode 3: FUSE via fusermount3 (compat fallback)

Some hardened distros (Debian stable defaults, RHEL ≤ 8 without changes)
disable unprivileged user namespaces. For those, onelf falls back to the
classic `fusermount3` setuid helper protocol: it socketpair-passes
`/dev/fuse` back from a `fusermount3` subprocess.

The mount lives in the host namespace in this mode, so it's visible to
other processes. Kernel still cleans up on process exit.

**Requirements:**
- `fusermount3` binary on `$PATH`.

## Mode 4: Ephemeral tmpfs (no FUSE available)

If `/dev/fuse` can't be opened at all (old kernel, locked-down container),
the runtime unshares a user+mount namespace, mounts a `tmpfs` at the
mountpoint, extracts the full package into it, and execs the entrypoint.

**Properties:**
- Invisible to the host (tmpfs is in our private mount namespace).
- Zero cleanup (namespace goes away on exit).
- Uses RAM equal to the uncompressed package size until the process exits.

**Requirements:**
- Unprivileged user namespaces enabled.

## Mode 5: Persistent cache (last resort)

If none of the above work, the runtime falls back to extracting under
`$XDG_CACHE_HOME/onelf/pkg/{id}/`. This is visible on disk and persists
between runs. A content-addressable store (`cas/`) deduplicates files
across packages.

**Properties:**
- First run is slower (full decompression to disk).
- Subsequent runs are instant (already extracted).
- Leaves persistent files on disk. Auto-GC removes packages unused for
  30 days (tunable via `ONELF_GC_MAX_AGE`).

**Requirements:**
- Writable home directory. That's it.

## Comparison

| Mode | First run | Next run | Visible | Persistent | Deps |
|------|-----------|----------|---------|------------|------|
| memfd | fast | fast | no | no | kernel |
| fuse (namespace) | fast | fast | no | no | `/dev/fuse` + userns |
| fuse (fusermount3) | fast | fast | yes (mount) | no | `fusermount3` |
| tmpfs | slow (full extract to RAM) | slow again | no | no | userns |
| cache | slow (extract to disk) | fast | yes | yes | writable $HOME |

## How the entrypoint is launched

Regardless of which mode is active, the runtime picks one of three exec
strategies based on the entrypoint's `PT_INTERP`:

1. **Direct exec (preferred).** When `bundle-libs` has rewritten the
   entrypoint's `PT_INTERP` to a path relative to the AppDir, the
   runtime chdirs into the AppDir and `execve`s the target directly.
   The kernel loads the bundled loader via the relative PT_INTERP, and
   `/proc/self/exe` points at the real binary — what Python, Electron,
   and Qt read to find their bundled resources.

2. **Userland-execve.** For PIE (`ET_DYN`) binaries on systems that
   support it, the runtime can map the bundled loader into the current
   process and jump to its entry, skipping the kernel loader entirely.

3. **Loader invocation (fallback).** When the target's `PT_INTERP` is
   absolute (unpatched) but a matching bundled loader exists, the
   runtime invokes the loader explicitly:
   `ld-linux-x86-64.so.2 --library-path ... --argv0 NAME target`.
   The bundle still runs, but `/proc/self/exe` will be the loader
   rather than the target. Apps that read `/proc/self/exe` to locate
   their own resources won't find them on this path. Re-run
   `bundle-libs` with an up-to-date `onelf` to get the direct-exec
   behavior.

## Forcing a mode

Set `ONELF_MODE` in the environment:

```bash
# Skip memfd, go straight to fuse
ONELF_MODE=fuse ./myapp.onelf

# Useful on old CI where kernel features are limited
ONELF_MODE=cache ./myapp.onelf
```

If the forced mode fails, the runtime errors out instead of falling back.
