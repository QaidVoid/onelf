# Introduction

**onelf** packages a Linux application and its shared libraries into a single
executable file. You ship one file, the user runs it, and everything works
without a system-wide installation or distro-specific packages.

## How it compares

| | onelf | AppImage | Docker | Static binary |
|---|---|---|---|---|
| One-file distribution | ✓ | ✓ | | ✓ |
| No install required | ✓ | ✓ | | ✓ |
| Bundles dynamic libs | ✓ | ✓ | ✓ | |
| No visible mount/extract | ✓ | | | ✓ |
| No external helper at runtime | ✓ | `fusermount3` | `runc` etc. | |
| Cross-libc (musl ↔ glibc) | ✓ | difficult | ✓ | |
| Built-in delta updates | ✓ | `appimageupdatetool` | registry | |
| Can bundle GUI apps | ✓ | ✓ | ✓ | limited |

## What a packed binary looks like

```
┌─────────────────────────────────────┐
│ onelf-rt (static musl runtime)      │  670 KB slim, 2 MB with updates
├─────────────────────────────────────┤
│ Manifest (zstd-compressed)          │  File tree, entrypoints, metadata
├─────────────────────────────────────┤
│ Payload                             │  File contents in 256 KB zstd blocks
│  - block 0                          │
│  - block 1                          │
│  - ...                              │
├─────────────────────────────────────┤
│ Footer (76 bytes, fixed)            │  Offsets + magic
└─────────────────────────────────────┘
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
