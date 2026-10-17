# Execution Modes

When you run a packed file, the runtime tries a ladder of modes and uses
the first one the host supports. Every rung but the last leaves no copy
of the package behind when the app exits.

The rules this page encodes:

- ✅ Let the ladder run. It takes the first mode the host supports and
  cleans up after itself: every rung but the cache removes its copy of
  the package when the app exits.
- ❌ Do not opt into the cache casually. It is the one mode that leaves
  the package's full size on disk, which is why the runtime never
  enters it without a request.
- ✅ Let a second launch of a running package share the first one's
  mount. It skips mounting and decompressing again, and
  `ONELF_FUSE_NO_SHARE=1` turns it off.
- ⚠️ A `fusermount3` mount outlives a server killed outright. Nothing
  tears it down at the time; the next launch of any package sweeps it.
- ✅ Force a mode with `ONELF_MODE` when you need one pinned. A forced
  mode that fails is an error, not a fallback.

```bash
ONELF_MODE=fuse    ./myapp.onelf
ONELF_MODE=tmpfs   ./myapp.onelf
ONELF_MODE=rundir  ./myapp.onelf
ONELF_MODE=memfd   ./myapp.onelf
ONELF_MODE=cache   ./myapp.onelf
```

The default is "try them in order, fall back on failure".

## The ladder

```
memfd                 static single binary
userns + FUSE         invisible, kernel teardown, lazy
fusermount3 + FUSE    host-visible, lazy, swept by any next launch
userns + tmpfs        invisible, eager
runtime directory     private dir, eager, cleaned on exit
persistent cache      only on request
```

## Mode 1: memfd (fastest, most limited)

✅ The fastest rung, and the most limited. When available, the package
is decompressed straight into an anonymous `memfd` file descriptor and
executed via `/proc/self/fd/N`. Nothing touches the filesystem. No
mount, no extraction, no temp dir.

**Requirements:**

- It is the default entrypoint. Other entrypoints start at the next
  rung.
- Its binary is an ELF with no `DT_NEEDED` entries, or the publisher
  set `[package] memfd = true` (`--memfd`) at pack time. A binary that
  `bundle-libs` linked against `libonelf-env.so` is refused at pack
  time, because that library exists only inside the package.

onelf auto-detects static binaries (no `DT_NEEDED`) and marks them
memfd-eligible. You'll see `[memfd]` next to the entrypoint in `onelf
info`. The binary is checked against its recorded hash before it runs,
and a binary that fails the check falls through to the next rung.

⚠️ There is no package tree in this mode. `ONELF_DIR` is empty and the
app cannot read any other file from the package. The recipe's `[env]` is
applied, except a value that names `${ONELF_DIR}`, which would point at
the filesystem root. That includes the default `bin/` prefix on `PATH`,
so `PATH` stays as the caller set it. `[paths]` is not applied: it makes
a directory of the package answer for an absolute path, and no directory
of the package exists on disk here to mount.

## Mode 2: FUSE via user namespace (default for most packages)

✅ The default for most packages, and the rung to prefer. The runtime
unshares a user+mount namespace, opens `/dev/fuse` directly, and mounts
the package as a private read-only filesystem. The entrypoint is
`exec`'d from the mount.

Why:

- ✅ No external helpers. No `fusermount3` in PATH required.
- ✅ The mount is **invisible** to other processes on the host. Check
  `mount | grep onelf` and you won't see anything.
- ✅ When the last process in the user namespace exits, the kernel tears
  the mount down automatically. Zero cleanup code.
- ✅ On-demand block decompression. The runtime only decompresses the
  blocks it reads, so startup is fast even for huge packages.

**Requirements:**

- Kernel with FUSE support (essentially every modern Linux).
- Unprivileged user namespaces enabled
  (`/proc/sys/kernel/unprivileged_userns_clone` is `1` or missing).
  ⚠️ Ubuntu 23.10 and later restrict them through AppArmor for binaries
  without an installed profile, which a downloaded file never has, so
  Ubuntu desktops usually land on the next rung.
- The package was not built with `needs-setuid`, and
  `ONELF_FUSE_NO_NAMESPACE` is not set. Either one sends the launch
  straight to `fusermount3`, because a setuid bit does nothing inside a
  user namespace.

## Mode 3: FUSE via fusermount3 (compat fallback)

☑️ The compat fallback where user namespaces are unavailable: the
classic `fusermount3` setuid helper protocol, with `/dev/fuse`
socketpair-passed back from a `fusermount3` subprocess.

The mount lives in the host namespace in this mode, so it is visible to
other processes. It is still lazy, which matters for large packages.

⚠️ A server that is killed outright cannot unmount what it served, and
the kernel does not do it either. Such a mount answers every request
with a disconnected transport. Every launch of any package sweeps the
private runtime directory for those: a mountpoint whose owner lock is
free and whose filesystem reports the disconnect is lazily unmounted
and removed. A mount whose owner is alive is never touched.

**Requirements:**

- `fusermount3` on `$PATH`. Without it, FUSE 2's `fusermount` is used
  when it is setuid root, since it passes `/dev/fuse` back the same way.
  `FUSERMOUNT_PROG` names a helper outright.

## Instances share a mount

✅ A second launch of a package that is already running does not mount
it again. It runs from the first launch's mount, served by the same
process, so nothing the first instance has read is decompressed a second
time, and the kernel's cache of the package's pages is shared between
them. This holds on both FUSE rungs.

How it works:

- The first launch mounts the package and starts a server for it that
  belongs to no one instance. The server records itself in the private
  runtime directory, in `.onelf-<name>-<id>.share`, under the package's
  whole id. The id is a hash of the manifest, which covers the content of
  every file and the runtime.
- Every instance holds a lease, `.onelf-<name>-<id>.lease`, which the app
  and everything it starts inherit. The server leaves once nobody holds
  the lease and no process runs out of the mount. It first asks a second
  after the launch that started it runs its app, and then whenever the
  mount has gone five seconds without a request. It takes its mount, its
  record and its lease with it.
- Launches that start together take turns on the record. The first one
  holds it until its app runs, so the others find a finished mount and
  never start a second server.
- On the `fusermount3` rung the mount is in the host's namespace, and a
  later launch runs from it as it is. On the namespace rung a later launch
  enters the server's user and mount namespace, and then a copy of that
  mount namespace of its own, so the host libraries it places over bundled
  ones stay its own.

A launch mounts the package for itself instead when:

- the library links it resolved differ from those the mount shows,
  because the host changed or `ONELF_NO_RESOLVER` is set. It says so on
  its error output;
- on the namespace rung, it would not see the same files there: it sees
  other mounts than the server does, which a launch from inside another
  app's namespace or a container does, and so does one after the host
  mounted something that it does not propagate into copies; its root is
  not `/`; or its working directory is not the same directory there;
- it stays out of user namespaces (`needs-setuid` or
  `ONELF_FUSE_NO_NAMESPACE`) and the running instance mounted in one;
- `ONELF_TRACE` is set, since the trace is recorded by the server;
- `ONELF_FUSE_NO_SHARE` is set to anything other than empty or `0`.

A package file replaced on disk, as a [self-update](./self-update) does,
has a new id. Its launches get a server of their own, and instances
started from the old file keep reading the old contents.

⚠️ A process that inherited the lease keeps the server running even when
it runs nothing from the package, as a host program the app started can.
A daemon that closes what it inherited is seen only while it runs a file
from the package or sits in its directory.

## Mode 4: Ephemeral tmpfs (no FUSE available)

☑️ For hosts where `/dev/fuse` can't be opened at all (old kernel,
locked-down container). The runtime unshares a user+mount namespace,
mounts a `tmpfs` at the mountpoint, extracts the full package into it,
and execs the entrypoint.

**Properties:**

- Invisible to the host (tmpfs is in our private mount namespace).
- Zero cleanup (namespace goes away on exit).
- ⚠️ Uses RAM equal to the uncompressed package size until the process
  exits.

**Requirements:**

- Unprivileged user namespaces enabled.
- The package was not built with `needs-setuid`. Such a package skips
  this rung, and `ONELF_MODE=tmpfs` on one is an error that says so.

## Mode 5: Runtime directory (no namespaces, no helper)

☑️ The last rung that still works anywhere. Without namespaces and
without `fusermount3` there is nothing left to mount with, so the
runtime extracts the package into its private per-user directory and
runs it from there. On a systemd host that directory is
`$XDG_RUNTIME_DIR`, a RAM-backed tmpfs that is cleared at logout.
Without one it is `/tmp/onelf-<uid>`, created with mode `0700`.

**Properties:**

- ✅ Needs nothing from the host beyond a writable private directory.
- Visible to other processes of the same user, like any file there.
- ✅ Each launch extracts its own tree, named after the package with the
  process id appended. Two launches of one package never share a tree
  or edit each other's files.
- The tree is removed when the last process using it exits. An app
  that leaves processes running after it exits keeps its tree until
  the last of them is gone. A launch killed before it could clean up
  leaves a tree that the next launch of any package removes.
- Every block is verified against its recorded hash before the
  entrypoint runs, as in every other mode.
- Uses space in the runtime directory equal to the uncompressed package
  size. A package larger than the free space fails with a message
  naming the directory and the sizes, and the ladder moves on.

## Mode 6: Persistent cache (only on request)

☑️ Instant later launches at the price of the package's full size kept
on disk. The cache extracts under `$XDG_CACHE_HOME/onelf/pkg/{id}/` and
keeps the extraction between runs. A content-addressable store (`cas/`)
deduplicates files across packages, and auto-GC removes packages unused
for 30 days (tunable via `ONELF_GC_MAX_AGE`).

Because it is the one mode that leaves something behind, the runtime
never falls into it on its own. It runs when one of three things asks
for it:

- the publisher packed with `--cache`, or `[package] cache = true` in
  the recipe;
- the user set `ONELF_CACHE=1` for this launch;
- the mode was forced with `ONELF_MODE=cache`.

Otherwise a host where every earlier rung fails gets an error saying
so, with `ONELF_CACHE=1` named as the way to allow the extraction.

## Comparison

| Mode | First run | Next run | Visible | Persistent | Deps |
|------|-----------|----------|---------|------------|------|
| memfd | fast | fast | no | no | kernel |
| fuse (namespace) | fast | fast | no | no | `/dev/fuse` + userns |
| fuse (fusermount3) | fast | fast | yes (mount) | no | `fusermount3` |
| tmpfs | slow (full extract to RAM) | slow again | no | no | userns |
| rundir | slow (full extract) | slow again | yes (files) | no | writable runtime dir |
| cache | slow (extract to disk) | fast | yes | yes | writable $HOME, and a request |

## How the entrypoint is launched

In every mode but memfd, the runtime first decides what the host
supplies (see [Bundling](./bundling#libraries-that-come-from-the-host)).
A host with no GL stack at all gets the build the package pins, if it
pins one (see [Pinning a GL build](./sysroot#pinning-a-gl-build-for-hosts-without-one)).
Then it picks an exec strategy based on what the binary needs:

1. ✅ **AT_EXECFN bootstrap (preferred).** `bundle-libs` injects a small
   bootstrap into bundled executables. At runtime the kernel execs
   the binary directly. The bootstrap reads `AT_EXECFN`, computes the
   bundled interpreter's path relative to the binary's own location,
   and jumps into it. `/proc/self/exe` points at the real binary,
   which is what Python, Electron, and Qt read to find their bundled
   resources. The binary's `$ORIGIN` run path finds the bundled
   libraries, and the host and set libraries linked beside them (see
   [Environment](./environment#library-paths)), so no
   `LD_LIBRARY_PATH` is set. When the host's glibc is newer than the
   bundled one, the runtime sets `ONELF_INTERP` to the host loader and
   the bootstrap jumps into that instead, for this process and every
   one it execs.

2. **Userland-execve.** For binaries without bootstrap injection that
   are PIE (`ET_DYN`), when the package records its bundled loader in
   `.onelf/interp`, the runtime maps that loader into the current
   process and jumps to its entry, skipping the kernel loader entirely.
   This is what brings up a glibc binary on a musl host, where the
   binary's `PT_INTERP` doesn't exist on disk.

3. ⚠️ **Loader invocation (fallback).** When neither of the above fits,
   the runtime invokes the bundled loader explicitly:
   `ld-linux.so --library-path ... --argv0 NAME target`. The bundle
   still runs, but `/proc/self/exe` will be the loader rather than
   the target. Apps that read `/proc/self/exe` for self-introspection
   may misbehave on this path.

4. **Self-extract fast path.** Binaries that embed their payload at
   end-of-file (pre-1.3.12 Bun, etc.) cannot tolerate
   `/proc/self/exe` pointing at the loader. The runtime detects them
   and arranges for a real kernel-exec of the binary itself. Inside a
   private mount namespace (namespace FUSE and tmpfs modes), it
   bind-mounts the bundled loader over the binary's PT_INTERP path. In
   the rundir and cache modes, it places a short symlink to the
   bundled loader in the private runtime directory and patches the
   binary's PT_INTERP in place to point at it. Where neither works,
   as on a `fusermount3` mount, it falls back to loader invocation.

## Forcing a mode

Set `ONELF_MODE` in the environment:

```bash
# Skip memfd, go straight to fuse
ONELF_MODE=fuse ./myapp.onelf

# Useful on old CI where kernel features are limited
ONELF_MODE=rundir ./myapp.onelf
```

A forced mode that fails errors out instead of falling back.
