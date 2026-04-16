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
4. Strips `RPATH`/`RUNPATH` from bundled binaries so the runtime can
   control `LD_LIBRARY_PATH`.

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
GL/DRI bundling. Same for Qt/GTK/Vulkan/Wayland. You can still force any of
these explicitly:

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
