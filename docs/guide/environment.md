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

The runtime walks the AppDir's `lib` subdirectories and prepends them to
`LD_LIBRARY_PATH`, keeps whatever the caller already had, and appends
host driver / system library dirs at the end:

```
LD_LIBRARY_PATH=<bundle lib dirs>:<previous LD_LIBRARY_PATH>:<host driver dirs>
```

Host driver dirs currently probed (each added only if it exists):

- `/run/opengl-driver/lib` (NixOS)
- `/run/opengl-driver-32/lib` (NixOS 32-bit)
- `/usr/lib/x86_64-linux-gnu` (Debian/Ubuntu multiarch)
- `/usr/lib64` (Fedora/RHEL/openSUSE)
- `/usr/lib`, `/lib/x86_64-linux-gnu`, `/lib64` (generic fallbacks)

This lets bundled apps find host-provided GPU userspace drivers (libcuda,
libvulkan, libGL, libva) on every distro without the user having to set
`LD_LIBRARY_PATH` manually. The bundle's own libs come first in the
search path, so they always win on name conflicts.

If the AppDir has `lib/dri/` or `lib/gbm/` it also sets:

- `LIBGL_DRIVERS_PATH` and `LIBVA_DRIVERS_PATH` to `lib/dri/`
- `GBM_BACKENDS_PATH` to `lib/gbm/`

If the AppDir has `share/vulkan/icd.d/` it sets:

- `VK_DRIVER_FILES` to the comma-joined list of ICD json files

If `share/glvnd/egl_vendor.d/` exists, `__EGL_VENDOR_LIBRARY_DIRS` is set.

If `share/libdrm/` exists, `LIBDRM_IDS_PATH` is set.

Finally, the package's own `share/` is prepended to `XDG_DATA_DIRS`, so
GLib/GTK discover bundled GSettings schemas and icon themes.

## Set by the user

| Variable | Effect |
|----------|--------|
| `ONELF_MODE` | Force a specific [execution mode](./execution-modes) |
| `ONELF_GC_MAX_AGE` | Cache GC threshold in days (default 30, `0` disables) |

## Portable directory redirection

If files named `<binary>.home`, `<binary>.config`, `<binary>.share`,
`<binary>.cache`, or `<binary>.env` exist next to the packed binary, the
runtime redirects the corresponding XDG env vars at them. See
[Portable Directories](./portable-dirs) for details.
