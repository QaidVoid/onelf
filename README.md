# onelf

Single-binary packaging for Linux. onelf packs an application and every
library it needs into one executable. The file runs from a private mount
that the kernel tears down on exit, takes the GPU drivers from the host,
and leaves nothing behind.

The full guide lives in [docs/](./docs).

## Features

- **One file, no install.** The runtime mounts the package with FUSE in a
  private user namespace, so no mount is visible on the host and no helper
  is needed. Where that is not possible the runtime falls back, in order,
  to a `fusermount3` mount, a tmpfs in a private namespace, and
  extraction into the per-user runtime directory. A persistent cache is
  opt-in.
- **Its own libc.** A bundle carries glibc and its loader, so it runs on
  musl hosts and on distributions older or newer than the build machine.
- **GPU drivers from the host.** Each library present both in the bundle
  and in the host's driver stack is chosen at launch by its symbol
  versions, never by directory order. Bundling Mesa is an explicit opt-in.
- **Bundles from a sysroot.** `bundle-libs` can fill a bundle from a
  pinned sysroot's package database instead of scanning your machine.
- **Signed delta updates.** Self-update straight from the release
  binary, downloading only the changed content, verified with an
  Ed25519 signature before anything is replaced. Or leave the updater
  out and let a package manager do it.
- **Recipes.** One `onelf.toml` captures the whole build, and the same
  inputs give the same bytes.

## Quick example

```sh
# Copy the binary into an AppDir and collect its shared libraries.
onelf bundle-libs ./myapp --from-binary /path/to/myapp --scan-dlopen

# Pack into a single file.
onelf pack ./myapp -o myapp.onelf --command bin/myapp

# Run it.
./myapp.onelf
```

GUI apps need no extra flags. Wayland and GTK data are bundled when the
binary uses them, and the GPU stack comes from the host. Pass `--gl`,
`--dri` or `--vulkan` only to ship Mesa for hosts that have no GL.

Or declare everything in `onelf.toml` and use `onelf build`. See
[docs/guide/recipe.md](./docs/guide/recipe.md).

## Install

Download a static binary for `x86_64`, `aarch64` or `i686` from the
[releases page](https://github.com/QaidVoid/onelf/releases), or build
from source:

```sh
cargo build --release --bin onelf
```

The binary lands at `target/release/onelf`. Building needs a stable Rust
toolchain, a musl C compiler (`musl-gcc`) for the embedded runtime,
`llvm-tools` and a host `cc`. On NixOS, `nix develop` sets these up. See
[docs/guide/building-onelf.md](./docs/guide/building-onelf.md) for the
details and for `ONELF_RT_PATH`, which embeds a prebuilt runtime instead
of compiling one.

## Project layout

```
crates/
  onelf/           CLI packer
  onelf-format/    binary format, manifest and launch resolver
  onelf-rt/        runtime, static musl, embedded in every packed file
  onelf-sysroot/   sysroot package databases and closures
  onelf-payloads/  bootstrap and environment payloads injected into bundles
```

## Docs

- [Introduction](./docs/guide/introduction.md)
- [Quick start](./docs/guide/quick-start.md)
- [Bundling libraries](./docs/guide/bundling.md)
- [Execution modes](./docs/guide/execution-modes.md)
- [Self-update](./docs/guide/self-update.md)
- [CLI reference](./docs/reference/cli.md)
- [Recipe schema](./docs/reference/recipe-schema.md)
- [Example: Miniflux + PostgreSQL](./docs/guide/examples/miniflux.md)

## License

MIT
