# Introduction

**onelf** packages a Linux application and its shared libraries into a single
executable file. You ship one file, the user runs it, and everything works
without a system-wide installation or distro-specific packages.

If you want to know *why* it works the way it does before reading how,
start with [How onelf Thinks](./concepts). The short version: a bundle
ships its own libc and everything above it, takes only the GPU drivers
and NSS modules from the host, and decides at every launch, one library
at a time, which of the two copies is the newer one.

## How it compares

"AppImage" means two different things today. The classic one is built
with AppImageKit's `appimagetool` and the stock type 2 runtime, which
needs `libfuse2` on the host. "Anylinux" is the AppImage built with
pkgforge's `sharun`, `uruntime`, and their own Rust `appimagetool` that
defaults to DwarFS and the uruntime: it works on musl hosts, needs no
helper installed, and falls back to namespaces or extraction on its
own. They are listed separately because they behave
very differently. onelf's helper, `fusermount3`, is only a fallback for
hosts that deny user namespaces, and its persistent cache is opt-in; the
classic runtime mounts under `/tmp/.mount_*` and leaves that directory
behind on a crash. For GPU drivers, onelf and Anylinux both bundle Mesa
and take the NVIDIA blob from the host. onelf then decides at every
launch, for each driver library the host also has, whether the host's
copy or the bundled one is the newer, by the symbol versions each
defines, and uses that one; the concepts page explains why.

|  | onelf | Anylinux | AppImage | Flatpak | Static |
| --- | --- | --- | --- | --- | --- |
| One file, no install | yes | yes | yes | no | yes |
| Bundles libraries | yes | yes | yes | shared runtime | no |
| musl hosts | yes | yes | no | no | yes |
| No helper needed | yes | yes | `fusermount` | `bwrap` | yes |
| Invisible mount | yes | no, reused | no | n/a | n/a |
| Nothing on disk | yes | `/tmp` fallback | mount dir on crash | installs | yes |
| GPU drivers | bundles Mesa, host copy if newer | bundles Mesa | host dirs on path | runtime extension | n/a |
| Bundle source | scan or package DB | `lib4bin`, `strace` | `linuxdeploy` | build in SDK | compiler |
| Delta updates | built in | zsync hook | external tool | OSTree | no |
| Sandbox | no | no | no | `bwrap`, as permitted | no |

The honest summary: the Anylinux AppImages are excellent single files
and the closest relative to onelf. Flatpak's shared runtime is shared in
principle; in practice apps pin different runtimes and versions, so a
machine ends up holding several of them at hundreds of megabytes each,
and the sandbox is as tight as the permissions an app asks for, which
browsers and anything that works on the whole filesystem tend to open
wide. Its real strength is the store and its update model. onelf's
difference is in two places. It never puts a host directory on the
library path, choosing each host library on its merits instead, and it
can take a bundle's contents from a package database rather than a scan
of the packer's machine.

## What a packed binary looks like

```
+-------------------------------------+
| onelf-rt (static musl runtime)      |  670 KB slim, 2 MB with updates
+-------------------------------------+
| Manifest (zstd-compressed)          |  File tree, entrypoints, metadata
+-------------------------------------+
| Payload                             |  File contents in 256 KB blocks (zstd or raw)
|  - block 0                          |
|  - block 1                          |
|  - ...                              |
+-------------------------------------+
| Footer (76 bytes, fixed)            |  Offsets + magic
+-------------------------------------+
```

When you execute the file:

1. The runtime reads its own footer to find the manifest and payload.
2. It picks the best [execution mode](./execution-modes). The default is a
   private user+mount namespace with FUSE.
3. The target entrypoint is `exec`'d from the mount. When it exits, the
   kernel tears the namespace down along with any mount. No cleanup code
   runs, and no filesystem artifacts remain.

## When to use onelf

- You want to distribute a Linux app as one file without `.deb`/`.rpm`/flathub/etc.
- You're okay with the 700 KB runtime overhead per package.
- You want the app to leave no trace when it exits.
- You want delta self-updates without bundling `zsync`.
- You want cross-libc portability (ship a musl binary, run on glibc hosts).

## When not to use onelf

- You need Windows or macOS support (Linux-only).
- You need sandboxing / containerization (onelf isn't a sandbox; see bubblewrap
  or flatpak for that).
- Your binary is already fully static and has zero dynamic-library deps. In
  that case you can just ship the binary directly.
