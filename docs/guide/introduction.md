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

"AppImage" means two different things today, so the table lists them
separately:

- **AppImage** is the classic one, built with AppImageKit's
  `appimagetool` and the stock type 2 runtime, which needs `libfuse2` on
  the host. It mounts under `/tmp/.mount_*` and leaves that directory
  behind on a crash.
- **AnyLinux AppImages** are AppImages built with pkgforge's tooling:
  `sharun` to bundle the libraries, `uruntime` as the runtime, and their
  own Rust `appimagetool`, which defaults to DwarFS. They work on musl
  hosts and need no helper installed. The runtime uses the host's setuid
  `fusermount` when there is one, falls back to a user namespace, and
  extracts to `/tmp` as a last resort, removing the copy on exit.

onelf needs no helper either. It mounts inside a private user namespace,
uses `fusermount3` only on hosts that deny user namespaces, and extracts
into the per-user runtime directory when neither works. Its persistent
cache is opt-in. Inside a user namespace, setuid helpers such as `sudo`
or `pkexec` do not work, so a package that runs them sets
`needs-setuid` and mounts through `fusermount3` instead. For GPU drivers, AnyLinux AppImages bundle Mesa and take
the NVIDIA blob from the host. onelf takes the whole stack from the host
by default. At every launch it decides, for each library the driver
needs that the bundle also carries, whether the host's copy or the
bundled one is newer, by the symbol versions each defines. Bundling Mesa
is a publisher opt-in for hosts that have no GL at all, and the concepts
page explains why.

|  | onelf | AnyLinux AppImage | AppImage | Flatpak | Static |
| --- | --- | --- | --- | --- | --- |
| One file, no install | yes | yes | yes | no | yes |
| Bundles libraries | yes | yes | yes | shared runtime | no |
| musl hosts | yes | yes | no | no | yes |
| No helper needed | yes | yes | `fusermount` | `bwrap` | yes |
| Invisible mount | yes, with user namespaces | no, `/tmp/.mount_*` | no | n/a | n/a |
| Instances share a mount | yes | yes | no | n/a | n/a |
| Nothing on disk | no copy left, cache opt-in | extraction removed on exit | mount dir on crash | installs | yes |
| GPU drivers | host, per library; bundling opt-in | bundled Mesa, NVIDIA from the host | host dirs on path | runtime extension | n/a |
| Bundle source | scan or package DB | `lib4bin` and `strace`, or debloated Arch packages | `linuxdeploy` | build in SDK | compiler |
| Delta updates | built in, signed | update server, unsigned | external tool | OSTree | no |
| Sandbox | no | no | no | `bwrap`, as permitted | no |
| Host programs the app starts | get the caller's environment back | bundle paths unset, home restored | inherit the bundle's | run in the sandbox | n/a |

The honest summary: AnyLinux AppImages are excellent single files and
the closest relative to onelf. Flatpak's shared runtime is shared in
principle. In practice apps pin different runtimes and versions, so a
machine ends up holding several of them at hundreds of megabytes each.
Its sandbox is as tight as the permissions an app asks for, and browsers
and anything that works on the whole filesystem tend to open it wide.
Flatpak's real strength is the store and its update model. onelf's
difference is in two places. It never puts a host directory on the
library path, nor sets one the app's children would inherit, choosing
each host library on its merits and showing it beside the bundled ones
instead, and it can take a bundle's contents from a package database
rather than a scan of the packer's machine.

## What a packed binary looks like

```
+-------------------------------------+
| onelf-rt (static musl runtime)      |  1.2 MB slim, 2.5 MB with the updater
+-------------------------------------+
| Manifest (zstd-compressed)          |  File tree, entrypoints, metadata
+-------------------------------------+
| Payload                             |  File contents in blocks, 256 KB by
|                                     |  default (zstd or raw)
|  - block 0                          |
|  - block 1                          |
|  - ...                              |
+-------------------------------------+
| Footer (76 bytes, fixed)            |  Offsets + magic
+-------------------------------------+
```

When you execute the file:

1. The runtime reads its own footer to find the manifest and payload.
2. It picks the best [execution mode](./execution-modes). On most hosts
   that is a FUSE mount inside a private user and mount namespace.
3. The target entrypoint is `exec`'d from the mount. When it exits, the
   kernel tears the namespace down along with the mount. No cleanup code
   runs, and no filesystem artifacts remain. The fallback modes for hosts
   without user namespaces are described on the same page.

## When to use onelf

- You want to distribute a Linux app as one file without `.deb`/`.rpm`/flathub/etc.
- You're okay with about 1.2 MB of runtime per package, or 2.5 MB with
  the self-updater.
- You want the app to leave no trace when it exits.
- You want delta self-updates from the release binary itself, with no
  extra files to publish.
- You want cross-libc portability (ship a musl binary, run on glibc hosts).

## When not to use onelf

- You need Windows or macOS support (Linux-only).
- You need sandboxing / containerization (onelf isn't a sandbox; see bubblewrap
  or flatpak for that).
- Your binary is already fully static and has zero dynamic-library deps. In
  that case you can just ship the binary directly.
