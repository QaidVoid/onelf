# Environment

The runtime sets a handful of environment variables before executing the
entrypoint. Packaged apps can read these to discover their own context.

## Set by the runtime

| Variable | Value | Example |
|----------|-------|---------|
| `ONELF_DIR` | Mount/extract root | `/run/user/1000/onelf-myapp-ab12cd34` |
| `ONELF_ACTIVE_MODE` | Active mode | `fuse`, `tmpfs`, `memfd`, `cache`, `dev` |
| `ONELF_ARGV0` | Original argv[0] | `myapp` |
| `ONELF_EXEC` | Path to the packed binary | `/home/alice/bin/myapp.onelf` |
| `ONELF_ENTRYPOINT` | Active entrypoint name | `myapp-daemon` |
| `ONELF_LAUNCH_DIR` | Caller's original cwd | `/home/alice/project` |

## Library paths

The runtime walks the AppDir's `lib` subdirectories and builds a
search-path string of the form:

```
<resolver link farm>:<bundle lib dirs>:<previous LD_LIBRARY_PATH>
```

That string is used two ways:

1. Passed to the bundled dynamic linker via `--library-path` when the
   runtime invokes it explicitly.
2. Set as `LD_LIBRARY_PATH` for AT_EXECFN-bootstrapped binaries, which
   reach the bundled libraries through their own run path and only need
   the farm from the environment, so that nested execs the app does
   still resolve correctly. A binary that got no `$ORIGIN` run path at
   bundle time (no slot and no `patchelf`) has no other way to the
   bundled libraries, so it receives the whole string instead.

The link farm holds the host libraries the runtime's resolver chose for
this launch: the driver stack the bundle does not carry, and any library
whose host copy is newer than the bundled one. See
[Bundling](./bundling#libraries-that-come-from-the-host) for how the
choice is made. No host directory is ever placed on the path, so a
library the bundle lacks and the resolver did not choose fails by name
instead of being satisfied from the host.

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

`QT_PLUGIN_PATH` names every `lib/qt6/plugins` and `lib/qt5/plugins`
the package and its [shared sets](./sysroot#sharing-a-dependency-set)
carry, unless the caller already set it. A relocatable Qt finds its
plugins relative to the executable, which holds while the toolkit sits
in the package, and stops holding the moment a set moves it elsewhere.
Qt then finds no platform plugin and aborts, and because it logs to the
journal rather than stderr the abort prints nothing at all.

Finally, the package's own `share/` is prepended to `XDG_DATA_DIRS`,
so GLib/GTK discover bundled GSettings schemas and icon themes.

## Data paths compiled into a library

The variables above cover the toolkits. An application that hardcodes an
absolute data path is a different matter, and the runtime cannot help it:
the path names the host's filesystem, so on a machine that happens to
have the same package installed the app silently reads the host's copy,
and on one that does not it fails. The bundled copy goes unused either
way.

The symptom is a package that works on the machine that built it and
fails in a container with a message about a missing file under `/usr`.
`libqalculate` reading `/usr/share/qalculate` is one example. Test in a
container that does not have the app installed, and where the app offers
an environment variable for the directory, set it from the recipe:

```toml
[env]
QALCULATE_DEFINITIONS_DIR = "${ONELF_DIR}/share/qalculate"
```

An app with no such variable needs a patched build, or a wrapper
entrypoint that arranges the path before exec.

## Custom environment variables

The recipe's `[env]` section lets the package declare extra env vars
the runtime should set. `${ONELF_DIR}` in values expands to the
package root at runtime, so paths follow the running app:

```toml
[env]
PYTHONHOME = "${ONELF_DIR}/python"
GST_PLUGIN_SYSTEM_PATH = "${ONELF_DIR}/lib/gstreamer-1.0"
```

`${ONELF_DIR}` and `$${VAR}` (escaped, expanded against the **live**
environment at runtime, with POSIX `${VAR:-word}` defaults) let values
prepend instead of replace. `PATH` defaults to
`${ONELF_DIR}/bin:$${PATH:-/usr/bin:/bin}`, so the package's `bin/` is
always on `PATH` (re-exec-safe), falling back to `/usr/bin:/bin` when
the inherited PATH is empty, unless `[env]` sets `PATH` itself.

See [Recipe File](./recipe#env) for details.

## Surviving a sandboxed re-exec

Some apps re-exec themselves with a wiped environment (Chromium and
Electron zygotes, Steam, `bwrap`-based sandboxes). Anything the runtime
exported via `LD_LIBRARY_PATH` or `[env]` is lost across that
`clearenv()` + `execve()`, because the runtime is no longer in the
loop on the re-exec.

onelf makes this survive by moving the guarantee into the ELF itself,
not the environment:

- **Libraries**: `bundle-libs` bakes an `$ORIGIN` `DT_RPATH` into every
  binary, the one entry that reaches `lib/` from the binary's own depth,
  so bundled libs resolve relative to its location on every exec. Executables that could not get one (no
  in-place slot and no `patchelf`, or self-extract binaries) are
  reported at pack time. They fall back to `LD_LIBRARY_PATH` and are
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
