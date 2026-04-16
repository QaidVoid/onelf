# onelf

Single-binary packaging for Linux. Packs a directory into a
self-extracting executable with three execution modes: in-memory
(`memfd`), FUSE mount, and on-disk cache extraction.

The full guide lives at [docs/](./docs).

## Quick example

```sh
# Collect shared libraries next to your binary.
onelf bundle-libs ./myapp --strip

# Pack into a single file.
onelf pack ./myapp -o myapp.bin --command bin/myapp

# Run it.
./myapp.bin
```

Graphical apps pick up GL, DRI, Vulkan, Wayland, and GTK bundling
automatically:

```sh
onelf bundle-libs ./myapp --strip
```

Or declare everything in `onelf.toml` and use `onelf build`. See
[docs/guide/recipe.md](./docs/guide/recipe.md).

## Install

From source:

```sh
cargo build --release
```

The binary lands at `target/release/onelf`. Building requires a musl
`gcc` for compiling the runtime stub. The build script looks up
`ONELF_MUSL_CC`, `CC_x86_64_unknown_linux_musl`, and
`x86_64-linux-musl-gcc` / `musl-gcc` in `PATH`. On NixOS, `nix
develop` sets all of these up.

To reuse a pre-built runtime (for `cargo install` etc.), set
`ONELF_RT_PATH=/path/to/onelf-rt` before building.

## Project layout

```
crates/
  onelf/         CLI packer
  onelf-format/  binary format types, no deps
  onelf-rt/      runtime stub, static musl, embedded in packed files
```

## Docs

- [Introduction](./docs/guide/introduction.md)
- [Quick start](./docs/guide/quick-start.md)
- [Bundling libraries](./docs/guide/bundling.md)
- [Execution modes](./docs/guide/execution-modes.md)
- [Recipe schema](./docs/reference/recipe-schema.md)
- [Example: Miniflux + PostgreSQL](./docs/guide/examples/miniflux.md)

## License

MIT
