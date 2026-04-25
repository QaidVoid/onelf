# Bundling Libraries

`onelf bundle-libs` walks the ELF files in an AppDir, resolves their
dependencies, and copies the shared libraries into `lib/`.

## Basic usage

```bash
onelf bundle-libs ./myapp
```

With no flags, this:

1. Scans every ELF file under `./myapp/` for `DT_NEEDED` entries.
2. Resolves each soname via ldconfig (or the NixOS store, when detected).
3. Copies the resolved `.so` files into `./myapp/lib/`.
4. Rewrites `RPATH`/`RUNPATH` on every bundled ELF to
   `$ORIGIN/../lib:$ORIGIN/../../lib:$ORIGIN/../../../lib`, so the
   bundled binaries find their libs relative to their own location
   without needing `LD_LIBRARY_PATH`. For binaries without an existing
   slot, `bundle-libs` falls back to `patchelf --set-rpath` (when
   patchelf is in `PATH`) to add a fresh entry.
5. Injects an AT_EXECFN bootstrap into each bundled executable. At
   runtime, the bootstrap reads `AT_EXECFN`, computes the bundled
   interpreter path relative to the binary's own location, and jumps
   into it. This keeps `/proc/self/exe` pointing at the real binary,
   which is important for Python's stdlib detection, Electron's ASAR
   locator, and Qt's plugin loader.
6. Scrubs `/usr/`, `/etc/`, `/nix/`, `/lib/`, and `/lib64/` strings
   inside the bundled dynamic loader to `/XXX/`, so it won't pick up
   the host's `ld.so.preload`, `ld.so.cache`, or hardcoded fallback
   library dirs.

## Self-extracting binaries (Bun, etc.)

Some binaries embed their payload at the end of the file. The most
common case is pre-1.3.12 Bun-compiled apps, which look for the
trailer `\n---- Bun! ----\n` at `-16` from end-of-file. These need
special handling. `bundle-libs` detects them automatically and:

- Skips bootstrap injection. The bootstrap appends bytes to the
  binary, which would clobber the trailer and break payload
  detection.
- Skips `patchelf --set-rpath` for binaries lacking a DT_RUNPATH
  slot. patchelf can grow the file.

At runtime, the onelf-rt arranges for the kernel to handle PT_INTERP
directly. In FUSE and tmpfs modes, it bind-mounts the bundled linker
over the binary's existing PT_INTERP path inside a private mount
namespace. In cache mode, it creates a short `/tmp` symlink and
patches the binary's PT_INTERP in-place. Either way,
`/proc/self/exe` resolves to the binary itself and Bun finds its
embedded JS bundle.

Bun 1.3.12 and newer uses a dedicated `.bun` ELF section instead of
the end-of-file trailer. Those binaries are unaffected by file-end
appending and get normal bootstrap injection treatment.

## Starting from a bare binary

```bash
onelf bundle-libs ./myapp --from-binary /usr/bin/myapp
```

Copies `/usr/bin/myapp` into `./myapp/bin/myapp`, then runs the normal flow.

## Detecting dlopen'd libs

Some libraries are loaded at runtime via `dlopen` and don't appear in
`DT_NEEDED`. `--scan-dlopen` searches the binary strings for common
candidates (GL, Wayland, Vulkan, X11, audio, DBus, and so on) and bundles
any matches.

```bash
onelf bundle-libs ./myapp --scan-dlopen
```

You can extend the allow-list with extra sonames:

```bash
onelf bundle-libs ./myapp --scan-dlopen --dlopen libmyvendor.so.1
```

## Framework auto-detection

If the binary has `DT_NEEDED` for `libGL.so.1`, onelf automatically enables
GL/DRI bundling. Same for Qt/GTK/Vulkan/Wayland. Detection also scans the
binary's byte content for literal soname strings, so frameworks that are
only `dlopen`'d at runtime (Blender loading `libwayland-cursor.so` after
checking `$XDG_SESSION_TYPE`, for example) get picked up too with no
`DT_NEEDED` entry required.

You can still force any of these explicitly:

```bash
onelf bundle-libs ./myapp --gl --vulkan --wayland --gtk
```

Auto-detected frameworks are printed so you know what was enabled.

## Extra library search paths

The default resolver walks ldconfig and the NixOS store. If the libraries
you want live somewhere else (cross-compile output, custom prefix), add a
search path:

```bash
onelf bundle-libs ./myapp --search-path /opt/custom/lib
```

`--search-path` takes precedence over ldconfig and the store, so it's the
best way to pin a specific library version.

## Packing on NixOS

NixOS ships a stub loader at `/lib64/ld-linux-x86-64.so.2` that exists
but refuses to run foreign binaries, printing
`NixOS cannot run dynamically linked executables...`. bundle-libs
handles this automatically:

- Any candidate loader whose canonical path contains `stub-ld` is
  rejected at every resolution tier, so a real glibc from the Nix
  store wins.
- A previously-bundled stub-ld accidentally copied into `lib/` is
  detected (by path or by the `NixOS cannot run` signature inside
  the file) and deleted on the next `bundle-libs` run.
- The bundled glibc loader has its `/nix/store/...` build-time paths
  scrubbed so it can't reach back to a store path that doesn't exist
  on another machine.

You shouldn't need to do anything special. If you see the stub-ld
error anyway, delete the AppDir's `lib/` and re-run `bundle-libs`
with the current `onelf`.

## Cross-libc hygiene

When the target binary is musl and the host is glibc (or vice versa),
bundle-libs can end up copying libraries built against the wrong libc.
Those fail at runtime with confusing "symbol not found" errors.

`--strict-libc` refuses to bundle libraries whose `DT_NEEDED` points at
the wrong libc family, and instead lists them under "Not found":

```bash
onelf bundle-libs ./myapp --strict-libc
```

Combine with `--search-path` pointing at the right-libc versions to make
the bundle clean.

## Excluding and including

```bash
onelf bundle-libs ./myapp --exclude libpthread,libdl
onelf bundle-libs ./myapp --include libsomething.so.1
```

`--exclude` skips libraries by prefix. `--include` forces a soname into
the resolution queue.

## Stripping

```bash
onelf bundle-libs ./myapp --strip
```

Runs `strip --strip-unneeded` on each copied library. Saves disk space
(often 20-40 %) at the cost of debuggability.

## Dry run

```bash
onelf bundle-libs ./myapp --dry-run
```

Shows what would be bundled without copying anything.

## GPU / graphics helpers

The granular framework flags:

| Flag | What it bundles |
|------|-----------------|
| `--gl` | `libGL.so`, `libEGL.so`, `libGBM`, `libGLX_mesa`, `libEGL_mesa` |
| `--dri` | Mesa DRI drivers (filtered to your architecture) |
| `--vulkan` | Vulkan ICD drivers + `libvulkan.so.1` |
| `--wayland` | `libwayland-*`, `libdecor-0`, `libxkbcommon`, Wayland client |
| `--gtk` | GSettings schemas under `share/glib-2.0/schemas` |

These are normally enabled automatically. Pass them manually to force-on
when auto-detection misses something.
