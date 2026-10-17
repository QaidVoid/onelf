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
| `LD_LIBRARY_PATH` | only for an ELF entrypoint without an `$ORIGIN` run path that the kernel starts | the bundled lib dirs, then the previous value |
| `ONELF_LIB_PATH` | `lib/` contains `.so` files | the bundled lib dirs, kept for scripts that export it as `LD_LIBRARY_PATH` |
| `GCONV_PATH` | `lib/gconv/` exists | `<pkg>/lib/gconv`, the bundled glibc's charset converters. Replaces a value the caller set. |
| `LIBGL_DRIVERS_PATH` | `lib/dri/` exists | `<pkg>/lib/dri` |
| `LIBVA_DRIVERS_PATH` | `lib/dri/` exists | `<pkg>/lib/dri` |
| `GBM_BACKENDS_PATH` | `lib/gbm/` exists | `<pkg>/lib/gbm` |
| `__EGL_VENDOR_LIBRARY_DIRS` | `share/glvnd/egl_vendor.d/` exists or host has one | merged list of dirs |
| `VK_DRIVER_FILES` | the package and the host both describe one Vulkan driver, or the package's own `libvulkan.so.1` loads, and the caller set none of `VK_DRIVER_FILES`, `VK_ICD_FILENAMES` and `VK_ADD_DRIVER_FILES` | colon-joined list of ICD files, one per driver; see [Vulkan drivers](../guide/environment#vulkan-drivers) |
| `LIBDRM_IDS_PATH` | `share/libdrm/` exists | that dir |
| `LIBDECOR_PLUGIN_DIR` | `share/libdecor/plugins-1/` exists | that dir. Replaces a value the caller set. |
| `DRIRC_CONFIGDIR` | `share/drirc.d/` exists | that dir |
| `XKB_CONFIG_ROOT` | `share/X11/xkb/` exists | that dir |
| `XDG_DATA_DIRS` | `share/` exists | `<pkg>/share` (prepended) |
| `FONTCONFIG_FILE` | the package or a set has `share/fonts/` | a generated config in the private runtime directory that includes the host's `fonts.conf`, or the caller's `FONTCONFIG_FILE` in its place, then the bundled font dirs |
| `QT_PLUGIN_PATH` | the package or a set has `lib/qt6/plugins` or `lib/qt5/plugins` | colon-joined list of every such dir, so relocatable Qt finds its platform plugin. Replaces a value the caller set. |
| `GIO_MODULE_DIR` | `lib/gio/modules/` exists in the package | that dir. It replaces GLib's default module directory, so the host's modules, built for the host's GLib, never load. Replaces a value the caller set. |
| `GIO_EXTRA_MODULES` | a set carries `lib/gio/modules/` too | colon-joined list of the sets' module dirs. Replaces a value the caller set. |
| `GTK_PATH` | `lib/gtk-3.0/` exists in the package or a set | colon-joined list of those dirs, for GTK 3's modules, theme engines and print backends. Replaces a value the caller set. |
| `GDK_PIXBUF_MODULE_FILE` | `lib/gdk-pixbuf-2.0/<version>/loaders.cache` exists | that cache, rewritten to absolute module paths (GTK resolves relative cache paths against the cwd). Replaces a value the caller set. |
| `GTK_IM_MODULE_FILE` | `lib/gtk-3.0/immodules.cache` exists | that cache, absolutised the same way, so the bundle's input-method modules are used. Replaces a value the caller set. |
| `GST_PLUGIN_SYSTEM_PATH_1_0`, `GST_PLUGIN_SYSTEM_PATH` | a library directory of the package or a set has `gstreamer-1.0/` | colon-joined list of those dirs. Replaces a value the caller set. |
| `GST_PLUGIN_SCANNER_1_0`, `GST_PLUGIN_SCANNER` | the bundle carries `gst-plugin-scanner` in `gstreamer-1.0/`, `gstreamer1.0/gstreamer-1.0/` or `libexec/gstreamer-1.0/` | that scanner. Replaces a value the caller set. |
| `PIPEWIRE_MODULE_DIR`, `SPA_PLUGIN_DIR` | a library directory has `pipewire-0.3/` or `spa-0.2/` | colon-joined list of those dirs. Replaces a value the caller set. |
| `ALSA_PLUGIN_DIR` | a library directory has `alsa-lib/` | the first such dir. Replaces a value the caller set. |
| `GI_TYPELIB_PATH` | a library directory has `girepository-1.0/` | colon-joined list of those dirs. Replaces a value the caller set. |
| `LIBHEIF_PLUGIN_PATH` | a library directory has `libheif/` | colon-joined list of their `plugins/` subdirs, or of the dirs themselves where they have none. Replaces a value the caller set. |
| `BABL_PATH`, `GEGL_PATH` | a library directory has `babl-0.1/` or `gegl-0.4/` | colon-joined list of those dirs. Replaces a value the caller set. |
| `TCL_LIBRARY`, `TK_LIBRARY` | a library directory or `share/tcltk/` has `tcl<version>/init.tcl` or `tk<version>/tk.tcl` | that dir. Replaces a value the caller set. |
| `ALSA_CONFIG_DIR`, `ALSA_CONFIG_PATH` | the package or a set has `share/alsa/` with `alsa.conf` | that dir and that file |
| `XLOCALEDIR` | `share/X11/locale/` exists | that dir |
| `MAGIC` | `share/file/misc/magic.mgc` or `share/misc/magic.mgc` exists | that file |
| `TEXTDOMAINDIR` | `share/locale/` exists | that dir |
| `LOCPATH` | a bundle lib dir has `locale/C.utf8/`, the host has no `/usr/lib/locale/locale-archive`, and the caller did not set it | colon-joined list of those `locale/` dirs |
| `SSL_CERT_FILE`, `CURL_CA_BUNDLE` | the host has no CA bundle at the usual paths, the package or a set carries one, and the caller set neither | the carried bundle |
| `TERMINFO_DIRS` | the package or a set has `share/terminfo/` or `lib/terminfo/` | the caller's list, or else the host's usual terminfo dirs, then the bundled ones |

The driver and data variables in this table keep a value the caller
set; the module variables replace it. Any of them that arrives pointing
into another onelf package is dropped first. See
[Which values the caller can override](../guide/environment#which-values-the-caller-can-override).

The runtime leaves `LD_LIBRARY_PATH` as the caller set it. Bundled
binaries find their libraries through their `$ORIGIN` run path, and the
host libraries the resolver chose and the libraries of the package's
shared sets are shown as symlinks in the bundle's library directory,
where that run path finds them. On an explicit linker invocation the
runtime also passes `--library-path`, which no child process inherits.
No host directory is ever searched: what the host supplies is chosen
per library by the resolver. See
[Environment](../guide/environment#library-paths).

The one exception is an ELF entrypoint without an `$ORIGIN` run path
that the kernel has to start, such as a self-extracting binary or an
executable `bundle-libs` could give no run path. It gets the bundled
lib dirs in `LD_LIBRARY_PATH`, since nothing else leads it to its
libraries, and the programs it runs inherit them.

A script entrypoint (shebang target) runs under a host interpreter that
must not see the bundled libraries, so it gets none of this. The
bundled binaries it runs find their libraries on their own.
`ONELF_LIB_PATH` is still published for scripts written to export it.

`VK_ADD_DRIVER_FILES` is never set by the runtime, but it is honored as
a guard, as are `VK_DRIVER_FILES` and `VK_ICD_FILENAMES`: if the app or
the user sets one, the runtime stays out of Vulkan's way entirely and
does not build `VK_DRIVER_FILES`.

## Private to onelf

The runtime sets these for the `onelf-env` exec hooks, which give a
program outside the package the environment the app was launched with
(see [Programs the app starts](../guide/environment#programs-the-app-starts)).
Neither reaches such a program, and neither is meant to be read or set
by anything else.

| Variable | Value |
|----------|-------|
| `ONELF_HOST_ENV` | One entry per variable the runtime set, changed or removed, in the form `NAME=<applied><original>;`. `<applied>` is the 64-bit FNV-1a hash of the value the runtime set, as 16 lowercase hex digits, `-` when the runtime removed the variable, or `*` to restore it whatever it holds (`ONELF_INTERP`, which the launch may set on the exec itself). `<original>` is `<len>:<bytes>`, the value the runtime was started with and its length in bytes, or `-` when it was unset. A hook restores a variable only while its value still hashes to `<applied>`. A runtime that inherits this variable applies it before anything else. |
| `ONELF_ROOTS` | The canonical package root and dependency set roots, `:`-separated. A program below one of them is the package's own and gets the app's environment unchanged. |

## Read by the runtime (user-settable)

| Variable | Effect |
|----------|--------|
| `ONELF_MODE` | Force a specific execution mode: `memfd`, `fuse`, `tmpfs`, `rundir` or `cache`. On failure the runtime errors instead of falling back. Read only; the mode chosen is reported as `ONELF_ACTIVE_MODE`, so a packed app that launches another does not force a mode on it. |
| `ONELF_GC_MAX_AGE` | Cache GC threshold in days (default 30; `0` disables auto-GC) |
| `ONELF_ENTRYPOINT` | Select the entrypoint by name, ahead of `argv[0]` and the default, unless it equals the inherited `ONELF_ENTRYPOINT_EXPORTED`. See [Entrypoints](../guide/entrypoints). |
| `ONELF_FUSE_NO_NAMESPACE` | Any value other than empty or `0` forces the `fusermount3` fallback path even when user namespaces are available, and keeps `[paths]` from being mounted. Useful for debugging mount visibility. |
| `FUSERMOUNT_PROG` | Path or name of the setuid helper the FUSE mode mounts through when it cannot use a user namespace, instead of `fusermount3` or a setuid `fusermount` on `PATH`. The AppImage runtime reads the same variable. |
| `ONELF_FUSE_NO_SHARE` | Any value other than empty or `0` gives this launch a FUSE mount and server of its own instead of running from the mount a running instance of the package serves. See [Instances share a mount](../guide/execution-modes#instances-share-a-mount). |
| `ONELF_FUSE_CACHE_BYTES` | Byte ceiling for the FUSE server's cache of decompressed blocks. Default 32 MiB; a value that does not parse or is zero keeps the default. A shared mount's server takes the value of the launch that started it. |
| `ONELF_CACHE` | `1` (any value other than empty or `0`) allows the persistent cache as the last execution mode for this launch. Without it, or `[package] cache = true` at pack time, the runtime stops at the runtime directory and reports failure rather than leaving an extraction on disk. |
| `ONELF_NO_RESOLVER` | `1` launches with nothing taken from the host, as if the package were packed with `host-libs = never`. The way to tell whether a failure is the resolver's doing. |
| `ONELF_LD_CACHE` | Path of the loader cache image the resolver reads instead of `/etc/ld.so.cache`, so a fixture can describe a host of its own. |
| `ONELF_TRACE` | Path of a file to record the sysroot path of every file the app opens, for capturing a trace to prune a sysroot build. Recording starts when the app is launched, so the runtime's own reads are left out. Appended, so runs accumulate. Records in `fuse` mode only, so set `ONELF_MODE=fuse` when capturing. A traced launch never shares a running instance's mount. |
| `ONELF_PROFILE` | Path of a file to record every block a run reads, as `path<TAB>offset` in the order first read, for `onelf pack --prefetch`. Recording starts with the mount, so the runtime's own reads before the launch are included. Appended, so runs accumulate. Records in `fuse` mode only. A profiled launch never shares a running instance's mount. See [Prefetching a startup](../guide/execution-modes#prefetching-a-startup). |
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
| `ONELF_PATCHELF` | Path to a `patchelf` binary, used by `bundle-libs` to add a fresh `DT_RUNPATH` to binaries without an existing slot and to inject the `onelf-env` constructor as a `DT_NEEDED` of every executable that runs under the bundled loader. Falls back to looking up `patchelf` in `PATH`. |
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
