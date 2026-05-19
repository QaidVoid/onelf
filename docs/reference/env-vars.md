# Environment Variables

Reference for all environment variables onelf reads or sets.

## Set by the runtime (visible to the packed app)

| Variable | Value |
|----------|-------|
| `ONELF_DIR` | Absolute path to the package root (FUSE mount, tmpfs, or cache dir). Empty string in `memfd` mode. |
| `ONELF_MODE` | `memfd`, `fuse`, `tmpfs`, `cache`, or `dev` (set by `onelf run`) |
| `ONELF_ARGV0` | Original `argv[0]` before multicall resolution |
| `ONELF_EXEC` | Absolute path to the packed binary |
| `ONELF_ENTRYPOINT` | Resolved entrypoint name |
| `ONELF_LAUNCH_DIR` | Original cwd at launch time |

## Library and GPU paths (auto-set if applicable)

| Variable | Set when | Points to |
|----------|----------|-----------|
| `LD_LIBRARY_PATH` | `lib/` contains `.so` files | `<pkg>/lib` followed by host driver paths |
| `LIBGL_DRIVERS_PATH` | `lib/dri/` exists | `<pkg>/lib/dri` |
| `LIBVA_DRIVERS_PATH` | `lib/dri/` exists | `<pkg>/lib/dri` |
| `GBM_BACKENDS_PATH` | `lib/gbm/` exists | `<pkg>/lib/gbm` |
| `__EGL_VENDOR_LIBRARY_DIRS` | `share/glvnd/egl_vendor.d/` exists or host has one | merged list of dirs |
| `VK_DRIVER_FILES` | `share/vulkan/icd.d/` has ICD JSONs | colon-joined list |
| `LIBDRM_IDS_PATH` | `share/libdrm/` exists | that dir |
| `LIBDECOR_PLUGIN_DIR` | `share/libdecor/plugins-1/` exists | that dir |
| `DRIRC_CONFIGDIR` | `share/drirc.d/` exists | that dir |
| `XKB_CONFIG_ROOT` | `share/X11/xkb/` exists | that dir |
| `XDG_DATA_DIRS` | `share/` exists | `<pkg>/share` (prepended) |

`LD_LIBRARY_PATH` is set as a fallback for AT_EXECFN-bootstrapped
binaries that do not drive an explicit linker invocation. The runtime
also passes `--library-path` to the bundled linker on direct
invocations. Both mechanisms cover the bundled lib search; the env
var gets inherited by child processes the app spawns.

## Read by the runtime (user-settable)

| Variable | Effect |
|----------|--------|
| `ONELF_MODE` | Force a specific execution mode. On failure the runtime errors instead of falling back. |
| `ONELF_GC_MAX_AGE` | Cache GC threshold in days (default 30; `0` disables auto-GC) |
| `ONELF_FUSE_NO_NAMESPACE` | Force the `fusermount3` fallback path even when user namespaces are available. Useful for debugging mount visibility. |
| `XDG_RUNTIME_DIR` | Where to create mountpoint dirs (falls back to `/tmp`) |
| `XDG_CACHE_HOME` | Where the persistent cache mode stores packages (falls back to `$HOME/.cache`) |
| `XDG_DATA_HOME` | Where `onelf integrate` installs `.desktop` files and icons (falls back to `$HOME/.local/share`) |

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

## Read by `onelf` at pack time

| Variable | Effect |
|----------|--------|
| `ONELF_PATCHELF` | Path to a `patchelf` binary, used by `bundle-libs` to add a fresh `DT_RUNPATH` to binaries without an existing slot and to inject the re-exec-safe `onelf-env` constructor as a `DT_NEEDED` on the entrypoint. Falls back to looking up `patchelf` in `PATH`. |
| `SOURCE_DATE_EPOCH` | Clamp file mtimes for reproducible output. See [Reproducible Builds](../guide/reproducible). |
