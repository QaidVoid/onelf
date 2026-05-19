# Environment

The runtime sets a handful of environment variables before executing the
entrypoint. Packaged apps can read these to discover their own context.

## Set by the runtime

| Variable | Value | Example |
|----------|-------|---------|
| `ONELF_DIR` | Mount/extract root | `/run/user/1000/onelf-myapp-ab12cd34` |
| `ONELF_MODE` | Active mode | `fuse`, `tmpfs`, `memfd`, `cache`, `dev` |
| `ONELF_ARGV0` | Original argv[0] | `myapp` |
| `ONELF_EXEC` | Path to the packed binary | `/home/alice/bin/myapp.onelf` |
| `ONELF_ENTRYPOINT` | Active entrypoint name | `myapp-daemon` |
| `ONELF_LAUNCH_DIR` | Caller's original cwd | `/home/alice/project` |

## Library paths

The runtime walks the AppDir's `lib` subdirectories and builds a
search-path string of the form:

```
<bundle lib dirs>:<previous LD_LIBRARY_PATH>:<host driver dirs>
```

That string is used two ways:

1. Passed to the bundled dynamic linker via `--library-path` when the
   runtime invokes it explicitly.
2. Set as `LD_LIBRARY_PATH` so AT_EXECFN-bootstrapped binaries find
   the bundled libs through the kernel-loaded interpreter, and so any
   nested execs the app does (where the runtime is not in the loop)
   still resolve correctly.

Host driver dirs probed (each added only if it exists):

- `/run/opengl-driver/lib` (NixOS)
- `/run/opengl-driver-32/lib` (NixOS 32-bit)
- `/usr/lib/x86_64-linux-gnu` (Debian/Ubuntu multiarch)
- `/usr/lib64` (Fedora/RHEL/openSUSE)
- `/usr/lib`, `/lib/x86_64-linux-gnu`, `/lib64` (generic fallbacks)

This lets bundled apps find host-provided GPU userspace drivers
(libcuda, libvulkan, libGL, libva) on every distro without the user
having to set `LD_LIBRARY_PATH` manually. The bundle's own libs come
first in the search path, so they always win on name conflicts.

If the AppDir has `lib/dri/` or `lib/gbm/` it also sets:

- `LIBGL_DRIVERS_PATH` and `LIBVA_DRIVERS_PATH` to `lib/dri/`
- `GBM_BACKENDS_PATH` to `lib/gbm/`

If the AppDir has `share/vulkan/icd.d/` it sets:

- `VK_DRIVER_FILES` to the colon-joined list of ICD json files

The runtime also auto-sets a few more vars when the corresponding
data directory exists:

- `__EGL_VENDOR_LIBRARY_DIRS` for `share/glvnd/egl_vendor.d/`
- `LIBDRM_IDS_PATH` for `share/libdrm/`
- `LIBDECOR_PLUGIN_DIR` for `share/libdecor/plugins-1/`
- `DRIRC_CONFIGDIR` for `share/drirc.d/`
- `XKB_CONFIG_ROOT` for `share/X11/xkb/`

Finally, the package's own `share/` is prepended to `XDG_DATA_DIRS`,
so GLib/GTK discover bundled GSettings schemas and icon themes.

## Custom environment variables

The recipe's `[env]` section lets the package declare extra env vars
the runtime should set. `${ONELF_DIR}` in values expands to the
package root at runtime, so paths follow the running app:

```toml
[env]
PYTHONHOME = "${ONELF_DIR}/python"
QT_PLUGIN_PATH = "${ONELF_DIR}/lib/qt6/plugins"
```

`${ONELF_DIR}` and `$${VAR}` (escaped, expanded against the **live**
environment at runtime) let values prepend instead of replace.
`PATH` defaults to `${ONELF_DIR}/bin:$${PATH}` — the package's `bin/`
is always on `PATH` (re-exec-safe) unless `[env]` sets `PATH` itself.

See [Recipe File](./recipe#env) for details.

## Surviving a sandboxed re-exec

Some apps re-exec themselves with a wiped environment (Chromium and
Electron zygotes, Steam, `bwrap`-based sandboxes). Anything the runtime
exported via `LD_LIBRARY_PATH` or `[env]` is lost across that
`clearenv()` + `execve()`, because the runtime is no longer in the
loop on the re-exec.

onelf makes this survive by moving the guarantee into the ELF itself,
not the environment:

- **Libraries**: `bundle-libs` bakes an `$ORIGIN/../lib` `DT_RUNPATH`
  into binaries, so bundled libs resolve relative to the binary's own
  location on every exec. Executables that could not get one (no
  in-place slot and no `patchelf`, or self-extract binaries) are
  reported at pack time — they fall back to `LD_LIBRARY_PATH` and are
  not re-exec-safe.
- **`[env]` and `preload`**: a tiny freestanding `onelf-env`
  constructor is bundled into `lib/` and injected as a `DT_NEEDED` of
  the entrypoint. Because `DT_NEEDED` lives in the ELF and is resolved
  via the `$ORIGIN` RUNPATH above, it loads on *every* exec; its
  constructor re-applies `.onelf/env` and `.onelf/preload` before
  `main()`, no matter how the app cleared the environment.

The `onelf-env` injection requires `patchelf` at pack time (set
`ONELF_PATCHELF` to override its location) and a prebuilt `onelf-env`
blob for the target architecture. When either is missing, packing
prints a warning and `[env]` is applied only on the first launch (the
runtime layer), not after a re-exec.

## Set by the user

| Variable | Effect |
|----------|--------|
| `ONELF_MODE` | Force a specific [execution mode](./execution-modes) |
| `ONELF_GC_MAX_AGE` | Cache GC threshold in days (default 30, `0` disables) |
| `ONELF_FUSE_NO_NAMESPACE` | Force the `fusermount3` path even when user namespaces work |

## Portable directory redirection

If files named `<binary>.home`, `<binary>.config`, `<binary>.share`,
`<binary>.cache`, or `<binary>.env` exist next to the packed binary, the
runtime redirects the corresponding XDG env vars at them. See
[Portable Directories](./portable-dirs) for details.
