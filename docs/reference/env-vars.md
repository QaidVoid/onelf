# Environment Variables

Reference for all environment variables onelf reads or sets.

## Set by the runtime (visible to the packed app)

| Variable | Value |
|----------|-------|
| `ONELF_DIR` | Absolute path to the package root (FUSE mount, tmpfs, runtime directory, or cache dir). Empty string in `memfd` mode. |
| `ONELF_ACTIVE_MODE` | `memfd`, `fuse`, `tmpfs`, `rundir`, `cache`, or `dev` (set by `onelf run`) |
| `ONELF_INTERP` | Set only when the host's glibc is newer than the bundled one: the host loader every bootstrapped binary in the process tree runs under. Another packed app launched from that tree drops the inherited value and decides for itself. Ignored by a set-uid, set-gid, or capability-raised executable, as glibc ignores `LD_PRELOAD` there |
| `ONELF_ARGV0` | Original `argv[0]` before multicall resolution |
| `ONELF_EXEC` | Absolute path to the packed binary |
| `ONELF_ENTRYPOINT` | Resolved entrypoint name |
| `ONELF_ENTRYPOINT_EXPORTED` | The same name, so a package the app launches ignores an inherited `ONELF_ENTRYPOINT` when it selects its own entrypoint |
| `ONELF_LAUNCH_DIR` | Original cwd at launch time |

`onelf run` sets `ONELF_DIR` (the AppDir), `ONELF_ACTIVE_MODE=dev`,
`ONELF_ARGV0`, `ONELF_EXEC`, `ONELF_ENTRYPOINT` and
`ONELF_ENTRYPOINT_EXPORTED`, but not
`ONELF_LAUNCH_DIR`. When the recipe's `working-dir` moves the app to
another directory, it passes the original one as `ONELF_USER_CWD`.

## Library and GPU paths (auto-set if applicable)

| Variable | Set when | Points to |
|----------|----------|-----------|
| `LD_LIBRARY_PATH` | `lib/` contains `.so` files | the resolver's link farm of chosen host libraries, then `<pkg>/lib` on explicit linker invocations |
| `ONELF_LIB_PATH` | `lib/` contains `.so` files | the link farm, then the bundled lib dirs, for a script entrypoint to export as `LD_LIBRARY_PATH` |
| `GCONV_PATH` | `lib/gconv/` exists | `<pkg>/lib/gconv`, the bundled glibc's charset converters |
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
| `FONTCONFIG_FILE` | the package or a set has `share/fonts/` | a generated config in the private runtime directory that includes the host's `fonts.conf`, then the bundled font dirs. Only set when the caller has not set it. |
| `QT_PLUGIN_PATH` | the package or a set has `lib/qt6/plugins` or `lib/qt5/plugins` | colon-joined list of every such dir, so relocatable Qt finds its platform plugin. Only set when the caller has not set it. |
| `GIO_EXTRA_MODULES` | `lib/gio/modules/` exists in the package or a set | colon-joined list of those dirs, so GLib loads the bundle's GIO modules rather than the host's (whose GLib they do not match). Only set when the caller has not set it. |
| `GDK_PIXBUF_MODULE_FILE` | `lib/gdk-pixbuf-2.0/<version>/loaders.cache` exists | that cache, rewritten to absolute module paths (GTK resolves relative cache paths against the cwd). Only set when the caller has not set it. |
| `GTK_IM_MODULE_FILE` | `lib/gtk-3.0/immodules.cache` exists | that cache, absolutised the same way, so the bundle's input-method modules are used. Only set when the caller has not set it. |

`LD_LIBRARY_PATH` is set as a fallback for AT_EXECFN-bootstrapped
binaries that do not drive an explicit linker invocation. The runtime
also passes `--library-path` to the bundled linker on direct
invocations. Both mechanisms cover the bundled lib search; the env
var gets inherited by child processes the app spawns. No host directory
appears in either: what the host supplies is chosen per library by the
resolver and reached through its link farm.

A script entrypoint (shebang target) runs under a host interpreter that
must not see the bundled libraries, so it gets neither mechanism: the
path is published as `ONELF_LIB_PATH` instead, and the script exports it
as `LD_LIBRARY_PATH` itself before execing bundled binaries.

`VK_ADD_DRIVER_FILES` is never set by the runtime, but it is honored as
a guard: if the app or the user sets it, the runtime stays out of
Vulkan's way entirely and does not build `VK_DRIVER_FILES`, keeping the
Vulkan loader's append semantics and the host's GPU drivers reachable.

## Read by the runtime (user-settable)

| Variable | Effect |
|----------|--------|
| `ONELF_MODE` | Force a specific execution mode: `memfd`, `fuse`, `tmpfs`, `rundir` or `cache`. On failure the runtime errors instead of falling back. Read only; the mode chosen is reported as `ONELF_ACTIVE_MODE`, so a packed app that launches another does not force a mode on it. |
| `ONELF_GC_MAX_AGE` | Cache GC threshold in days (default 30; `0` disables auto-GC) |
| `ONELF_ENTRYPOINT` | Select the entrypoint by name, ahead of `argv[0]` and the default, unless it equals the inherited `ONELF_ENTRYPOINT_EXPORTED`. See [Entrypoints](../guide/entrypoints). |
| `ONELF_FUSE_NO_NAMESPACE` | Any value other than empty or `0` forces the `fusermount3` fallback path even when user namespaces are available, and keeps `[paths]` from being mounted. Useful for debugging mount visibility. |
| `ONELF_FUSE_CACHE_BYTES` | Byte ceiling for the FUSE server's cache of decompressed blocks. Default 32 MiB; a value that does not parse or is zero keeps the default. |
| `ONELF_CACHE` | `1` (any value other than empty or `0`) allows the persistent cache as the last execution mode for this launch. Without it, or `[package] cache = true` at pack time, the runtime stops at the runtime directory and reports failure rather than leaving an extraction on disk. |
| `ONELF_NO_RESOLVER` | `1` launches with nothing taken from the host, as if the package were packed with `host-libs = never`. The way to tell whether a failure is the resolver's doing. |
| `ONELF_LD_CACHE` | Path of the loader cache image the resolver reads instead of `/etc/ld.so.cache`, so a fixture can describe a host of its own. |
| `ONELF_TRACE` | Path of a file to record the sysroot path of every file the app opens, for capturing a trace to prune a sysroot build. Recording starts when the app is launched, so the runtime's own reads are left out. Appended, so runs accumulate. Records in `fuse` mode only, so set `ONELF_MODE=fuse` when capturing. |
| `ONELF_NO_PLATFORM_FETCH` | `1` stops the runtime from fetching the pinned GL build or dependency set on a host without one. A build already in the store is still used. The launch warns and continues. |
| `ONELF_PLATFORM_URL` | Fetch the pinned GL build from this URL instead of the package's. The pinned hash still has to match, so a mirror cannot substitute content. |
| `ONELF_PLATFORM_STORE` | Directory holding fetched GL builds and dependency sets instead of `platform/` under the cache root. `bundle-libs` also looks there for a set's build. |
| `ONELF_SET_MODE` | `mount` serves a dependency set from its stored build, the default; `extract` copies it out through the package cache. A host that cannot mount falls back to `extract`. |
| `XDG_RUNTIME_DIR` | The private per-user directory for mountpoints and rundir trees. Without a usable one the runtime creates `/tmp/onelf-<uid>` with mode `0700` |
| `XDG_CACHE_HOME` | The onelf cache lives in its `onelf/` subdirectory: the cache mode's packages and the fetched GL builds and dependency sets. Falls back to `$HOME/.cache`. A relative value is ignored |
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
| `SOURCE_DATE_EPOCH` | An integer Unix timestamp that clamps file mtimes for reproducible output. A value that is not an integer is ignored. `--mtime` overrides it. See [Reproducible Builds](../guide/reproducible). |
| `ONELF_PACK_CHUNK_BYTES` | In-flight compression chunk budget in bytes (default 64 MiB). Purely a memory bound: it caps how many files are compressed before their blocks are written back; output bytes are unaffected. A value that does not parse or is zero keeps the default. |
| `NO_COLOR` | When set (any value, per the NO_COLOR convention), disables the bundler's ANSI colorized status output in `bundle-libs` and `sysroot`. Color also requires a terminal stderr. |

## Build-time (compiling the `onelf` crate)

These are read by `onelf`'s build script, not by the tools or the
runtime. They exist to cross-compile the embedded runtimes and payloads
and to offer escape hatches for toolchain-less builds; a plain
`cargo build` on Linux needs none of them beyond the prerequisites in
[Building and Testing onelf](../guide/building-onelf).

| Variable | Effect |
|----------|--------|
| `ONELF_MUSL_CC` | Names the musl cross C compiler used to build the embedded `onelf-rt` runtimes (e.g. `x86_64-linux-musl-gcc`). When set, a miss is a hard error; without it the build script searches `CC_<triple>`, `<arch>-linux-musl-gcc` and `musl-gcc` on `PATH`, then common Nix and Bootlin locations, and fails with instructions if nothing is found. |
| `ONELF_RT_PATH` | Embed a prebuilt slim `onelf-rt` binary instead of building one from source. The file must exist and pass ELF and architecture checks. This is how `cargo install` builds without a musl toolchain. |
| `ONELF_RT_UPDATE_PATH` | Same, for the update-capable runtime variant. Defaults to `ONELF_RT_PATH` when unset, so a toolchain-less build ships the slim binary for both slots (self-update packages then carry a runtime without update code). |
| `ONELF_PAYLOAD_DIR` | Directory of prebuilt payload artifacts (`bootstrap_<arch>.bin`, `onelf_env_<arch>.so` for x86_64/aarch64/i686), embedded instead of compiling the payloads. Architectures with no file get an inert placeholder. |
| `ONELF_PAYLOAD_ALL` | When set (`=1`), build payloads for all three architectures instead of only the host's. Needs a working cross toolchain per architecture (`ONELF_PAYLOAD_CC_<ARCH>`). |
| `ONELF_PAYLOAD_CC_<ARCH>` | C compiler that links one architecture's freestanding payloads; suffixes are `X86_64`, `AARCH64`, `I686` (e.g. `ONELF_PAYLOAD_CC_AARCH64=/opt/bootlin/.../aarch64-linux-gcc`). A set name is a hard requirement; unset names fall back to the host `cc` for a native Linux host, then Bootlin paths. |
| `ONELF_OBJCOPY` | Path to an `objcopy` used to flatten the bootstrap payload ELF into a raw binary. Falls back to the rustc sysroot's `llvm-objcopy`, then `rust-objcopy`/`llvm-objcopy` on `PATH` (`rustup component add llvm-tools` supplies it). |
