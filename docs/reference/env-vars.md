# Environment Variables

Reference for all environment variables onelf reads or sets.

## Set by the runtime (visible to the packed app)

| Variable | Value |
|----------|-------|
| `ONELF_DIR` | Absolute path to the package root (FUSE mount, tmpfs, or cache dir). Empty string in `memfd` mode. |
| `ONELF_MODE` | `memfd`, `fuse`, `tmpfs`, `cache`, or `dev` |
| `ONELF_ARGV0` | Original `argv[0]` before multicall resolution |
| `ONELF_EXEC` | Absolute path to the packed binary |
| `ONELF_ENTRYPOINT` | Resolved entrypoint name |
| `ONELF_LAUNCH_DIR` | Original cwd at launch time |

## Library and GPU paths (auto-set if applicable)

| Variable | Set when | Points to |
|----------|----------|-----------|
| `LD_LIBRARY_PATH` | `lib/` contains `.so` files | `<pkg>/lib` (prepended) |
| `LIBGL_DRIVERS_PATH` | `lib/dri/` exists | `<pkg>/lib/dri` |
| `LIBVA_DRIVERS_PATH` | `lib/dri/` exists | `<pkg>/lib/dri` |
| `GBM_BACKENDS_PATH` | `lib/gbm/` exists | `<pkg>/lib/gbm` |
| `__EGL_VENDOR_LIBRARY_DIRS` | `share/glvnd/egl_vendor.d/` exists | that dir |
| `VK_DRIVER_FILES` | `share/vulkan/icd.d/` has ICD JSONs | colon-joined list |
| `LIBDRM_IDS_PATH` | `share/libdrm/` exists | that dir |
| `LIBDECOR_PLUGIN_DIR` | `share/libdecor/plugins-1/` exists | that dir |
| `DRIRC_CONFIGDIR` | `share/drirc.d/` exists | that dir |
| `XDG_DATA_DIRS` | `share/` exists | `<pkg>/share` (prepended) |

## Read by the runtime (user-settable)

| Variable | Effect |
|----------|--------|
| `ONELF_MODE` | Force a specific execution mode. On failure the runtime errors instead of falling back. |
| `ONELF_GC_MAX_AGE` | Cache GC threshold in days (default 30; `0` disables auto-GC) |
| `XDG_RUNTIME_DIR` | Where to create mountpoint dirs (falls back to `/tmp`) |
| `XDG_CACHE_HOME` | Where the persistent cache mode stores packages (falls back to `$HOME/.cache`) |

## Portable directory redirection

When the corresponding file exists next to the packed binary, the
runtime points the variable at it and moves the original value to
`REAL_*`:

| Sibling file | Redirects | Saved as |
|--------------|-----------|----------|
| `<binary>.home` | `HOME` | `REAL_HOME` |
| `<binary>.config` | `XDG_CONFIG_HOME` | `REAL_XDG_CONFIG_HOME` |
| `<binary>.share` | `XDG_DATA_HOME` | `REAL_XDG_DATA_HOME` |
| `<binary>.cache` | `XDG_CACHE_HOME` | `REAL_XDG_CACHE_HOME` |

See [Portable Directories](../guide/portable-dirs) for the full story.

## Build-time (SOURCE_DATE_EPOCH)

Read by `onelf pack` and `onelf build` to clamp file mtimes for
reproducible output. See [Reproducible Builds](../guide/reproducible).
