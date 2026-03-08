# onelf

Single-binary packaging tool for Linux. Packs a directory into a self-extracting executable with three runtime execution modes: in-memory (memfd), FUSE mount, and cache extraction.

## Features

- **Three execution modes** — memfd for static binaries, FUSE for on-demand mounting, cache for disk extraction
- **FUSE by default** — mounts the package as a read-only filesystem with no extraction overhead
- **Content-addressable cache** — BLAKE3-based deduplication with hardlinks across packages
- **Zstd compression** — configurable levels 0-22 with optional dictionary training
- **Multiple entrypoints** — multicall binary support via argv[0] resolution
- **Library bundling** — scan and copy shared library dependencies with GPU driver support
- **Cross-libc portability** — ELF interpreter detection and bundled interpreter fallback
- **Desktop integration** — embedded icons, desktop files
- **Portable mode** — optional per-binary config/data directories and env files
- **Portable runtime** — statically linked musl binary, no libc dependency via rustix

## Installation

### Build Requirements

- Rust (edition 2024)
- musl-gcc (for compiling the runtime stub)

The build script automatically finds musl-gcc by checking:
1. `ONELF_MUSL_CC` environment variable
2. `CC_x86_64_unknown_linux_musl` environment variable
3. `x86_64-linux-musl-gcc` or `musl-gcc` in PATH
4. `/nix/store` scanning (NixOS)

### Build

```sh
cargo build --release
```

The resulting binary is at `target/release/onelf`.

### NixOS

A flake dev shell is provided:

```sh
nix develop
cargo build --release
```

For static musl binaries:

```sh
nix develop
cargo build --release --target x86_64-unknown-linux-musl
```

### Pre-built Runtime

To skip building onelf-rt from source (e.g. for `cargo install` or `cargo publish`), set `ONELF_RT_PATH` to a pre-built runtime binary:

```sh
ONELF_RT_PATH=/path/to/onelf-rt cargo install --path crates/onelf
```

## Usage

### Bundle shared libraries

Collect shared library dependencies before packing:

```sh
onelf bundle-libs ./myapp --strip
```

For graphical applications, use the GPU and toolkit flags:

```sh
onelf bundle-libs ./myapp --strip --gl --dri --vulkan --wayland --gtk
```

Use `--dry-run` to preview what would be copied.

Full options:

```
onelf bundle-libs <DIRECTORY> [OPTIONS]

Options:
  --target <PATH>        Specific binary to analyze (default: scan all ELF files)
  --lib-dir <PATH>       Where to copy libs, relative to DIRECTORY [default: lib]
  --exclude <PATTERN>    Exclude libs matching prefix (comma-separated or repeatable)
  --include <SONAME>     Additional libraries by soname (comma-separated or repeatable)
  --search-path <PATH>   Additional library search directories (repeatable)
  --dry-run              Show what would be copied without copying
  --no-recursive         Don't resolve transitive dependencies
  --strip                Strip debug symbols from copied libraries

GPU and toolkit flags:
  --gl                   Bundle Mesa GL/EGL/GBM libraries
  --dri                  Bundle DRI drivers (architecture-filtered)
  --vulkan               Bundle Vulkan ICD drivers (architecture-filtered)
  --wayland              Bundle Wayland client libraries (libwayland, libdecor, libxkbcommon)
  --gtk                  Bundle GSettings schemas for GTK apps
```

### Pack a directory

```sh
onelf pack ./myapp -o myapp.bin --command bin/myapp
```

Full options:

```
onelf pack <DIRECTORY> -o <OUTPUT> --command <PATH> [OPTIONS]

Options:
  --name <NAME>                Package name (defaults to command basename)
  --entrypoint <name=path>     Additional entrypoints (repeatable)
  --default-entrypoint <NAME>  Default entrypoint name
  --lib-dir <PATH>             Library directories for LD_LIBRARY_PATH (repeatable)
  --level <0-22>               Zstd compression level [default: 12]
  --dict                       Build a shared zstd dictionary
  --memfd                      Mark default entrypoint as memfd-eligible
  --no-memfd                   Force cache mode (disable memfd)
  --working-dir <MODE>         Working directory: inherit, package, command [default: inherit]
  --update-url <URL>           Base URL for delta updates
  --exclude <PATTERN>          Exclude files matching glob patterns (repeatable)
```

### Inspect a package

```sh
onelf info myapp.bin    # Show metadata, entrypoints, compression stats
onelf list myapp.bin    # List all files with sizes and hashes
```

### Extract files

```sh
onelf extract myapp.bin                          # Extract all to ./onelf_extracted/
onelf extract myapp.bin -o ./out                 # Extract all to ./out/
onelf extract myapp.bin --file bin/myapp -o -    # Single file to stdout
```

### Extract metadata

```sh
onelf icon myapp.bin                             # Extract icon to stdout
onelf icon myapp.bin -o icon.png                 # Extract icon to file
onelf desktop myapp.bin                          # Extract desktop file to stdout
```

Icons are resolved in order: `.onelf/icons/{entrypoint}.svg`, `.onelf/icons/{entrypoint}.png`, `.onelf/icons/default.svg`, `.onelf/icons/default.png`.

Desktop files: `.onelf/desktop/{entrypoint}.desktop`, `.onelf/desktop/default.desktop`.

### Manage cache

```sh
onelf cache list              # List cached packages
onelf cache gc --max-age 7    # Remove packages unused for 7+ days
onelf cache clear             # Remove all cached data
```

## Execution Modes

When a packed binary runs, it tries these modes in order:

### 1. Memfd (in-memory)

Creates an anonymous file descriptor via `memfd_create`, decompresses the entrypoint into memory, and executes directly. No disk I/O. Only used when the entrypoint is marked memfd-eligible (`--memfd` flag) — intended for single static binaries with no shared library dependencies.

### 2. FUSE (default)

Mounts the package as a read-only FUSE filesystem and executes the entrypoint from the mount. Files are decompressed on demand with a block cache and sequential prefetch. The parent process serves FUSE requests while the child runs the application. Requires `fusermount3` in PATH. Reuses existing mounts from other instances.

### 3. Cache (fallback)

Extracts the package to `~/.cache/onelf/` using content-addressable storage. Files are stored by BLAKE3 hash and hardlinked into the package directory, providing deduplication across packages. Subsequent runs skip extraction if the cache is intact.

### Forcing a mode

Set `ONELF_MODE` to override the default selection:

```sh
ONELF_MODE=fuse  ./myapp.bin   # Force FUSE (error if unavailable)
ONELF_MODE=cache ./myapp.bin   # Force cache extraction
ONELF_MODE=memfd ./myapp.bin   # Force memfd execution
```

## Portable Mode

The runtime supports per-binary portable directories and environment files. Place these next to the binary:

| Path | Effect |
|---|---|
| `myapp.bin.home` | Redirects `HOME` to this directory |
| `myapp.bin.config` | Redirects `XDG_CONFIG_HOME` |
| `myapp.bin.share` | Redirects `XDG_DATA_HOME` |
| `myapp.bin.cache` | Redirects `XDG_CACHE_HOME` |
| `myapp.bin.env` | Loads `KEY=VALUE` lines (or `unset VAR`) |

Original values are preserved in `REAL_HOME`, `REAL_XDG_CONFIG_HOME`, etc.

Create portable directories from the binary itself:

```sh
./myapp.bin --onelf-portable          # Create all portable directories
./myapp.bin --onelf-portable-home     # Create just .home
```

## GPU and Graphics Support

When libraries are bundled with `bundle-libs`, the runtime auto-configures graphics environment variables:

| Condition | Variable Set |
|---|---|
| `lib/dri/` exists | `LIBGL_DRIVERS_PATH`, `LIBVA_DRIVERS_PATH` |
| `lib/gbm/` exists | `GBM_BACKENDS_PATH` |
| `share/glvnd/egl_vendor.d/` exists | `__EGL_VENDOR_LIBRARY_DIRS` |
| `share/drirc.d/` exists | `DRIRC_CONFIGDIR` |
| `share/libdrm/` exists | `LIBDRM_IDS_PATH` |
| `share/libdecor/plugins-1/` exists | `LIBDECOR_PLUGIN_DIR` |
| `share/vulkan/icd.d/*.json` exists | `VK_DRIVER_FILES` |
| `share/` exists | `XDG_DATA_DIRS` (prepended) |

## Desktop Integration

Packed binaries can include icons and desktop files for file manager integration.

### Embedding metadata

Place files in the source directory before packing:

```
.onelf/
  icons/
    default.svg          # or default.png
    myentrypoint.svg     # per-entrypoint override
  desktop/
    default.desktop
```

## Cross-libc Support

When packing glibc applications for use on musl systems (or vice versa), onelf can detect and bundle the ELF interpreter. The runtime will set up interpreter symlinks and fall back to the bundled interpreter if the system one is missing, using `--inhibit-cache`, `--library-path`, and `--argv0` for correct invocation.

## Environment Variables

### Set by the runtime

| Variable | Description |
|---|---|
| `ONELF_DIR` | Package root directory (empty for memfd mode) |
| `ONELF_ARGV0` | Original argv[0] |
| `ONELF_EXEC` | Path to the onelf binary |
| `ONELF_ENTRYPOINT` | Active entrypoint name |
| `ONELF_LAUNCH_DIR` | Working directory at launch |
| `ONELF_MODE` | Execution mode used (`memfd`, `fuse`, `cache`) |
| `LD_LIBRARY_PATH` | Prepended with bundled library directories |

### User-configurable

| Variable | Description |
|---|---|
| `ONELF_MODE` | Force execution mode: `memfd`, `fuse`, `cache` |
| `ONELF_ENTRYPOINT` | Force a specific entrypoint |
| `ONELF_GC_MAX_AGE` | Cache GC max age in days (default: 30, 0 to disable) |
| `XDG_RUNTIME_DIR` | FUSE mountpoint base (default: `/tmp`) |
| `XDG_CACHE_HOME` | Cache directory base (default: `~/.cache`) |

### Build-time

| Variable | Description |
|---|---|
| `ONELF_MUSL_CC` | Override musl-gcc path |
| `CC_x86_64_unknown_linux_musl` | Alternative musl-gcc path |
| `ONELF_RT_PATH` | Pre-built runtime binary (skips source build) |

## Binary Format

```
[Runtime ELF][Manifest (zstd)][Payload (blocks)][Dictionary?][Footer (76 bytes)]
```

- **Runtime ELF** — statically linked musl binary with `ONELF\0` signature in ELF header padding (bytes 9-14)
- **Manifest** — zstd-compressed directory tree, entrypoints, and string table with xxHash32 checksum
- **Payload** — concatenated zstd-compressed file blocks (256 KB block size)
- **Dictionary** — optional zstd dictionary for improved compression
- **Footer** — 76 bytes with magic numbers, offsets, sizes, and xxHash32 checksum

Package identity is a BLAKE3 hash of the serialized manifest.

## Project Structure

```
crates/
  onelf/          CLI packer tool
  onelf-format/   Binary format definitions (no dependencies)
  onelf-rt/       Runtime stub (compiled for musl, embedded in packed binaries)
```

## License

MIT
