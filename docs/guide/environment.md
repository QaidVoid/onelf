# Environment

The runtime sets a handful of environment variables before executing the
entrypoint. Packaged apps can read these to discover their own context.

The rules this page encodes:

- ✅ Read the app's context from the `ONELF_*` variables. The runtime sets
  them before the entrypoint runs.
- ✅ Set a data directory with `[env]` when the app offers a variable for
  it. A variable costs nothing and works everywhere.
- ☑️ Use `[paths]` only when there is no variable to set. It needs a
  private mount namespace, and two launch cases fall back with a message
  on the terminal.
- ⚠️ Test in a container that does not have the app installed. A
  hardcoded `/usr` data path silently reads the host's copy where the
  package happens to be installed and fails where it is not.
- ⚠️ The environment does not survive a `clearenv()` re-exec. onelf
  moves the guarantee into the ELF; without the `onelf-env` constructor,
  `[env]` applies only on the first launch.
- ✅ Start host programs as usual. A program outside the package gets
  the environment the app was launched with, not the app's.
- ⚠️ `system()`, `popen()` and the `execl` family pass the app's
  environment on unchanged. See [Programs the app starts](#programs-the-app-starts).

## Set by the runtime

| Variable | Value | Example |
|----------|-------|---------|
| `ONELF_DIR` | Mount/extract root (empty in memfd mode) | `/run/user/1000/onelf-myapp-ab12cd34` |
| `ONELF_ACTIVE_MODE` | Active mode | `memfd`, `fuse`, `tmpfs`, `rundir`, `cache`, or `dev` under `onelf run` |
| `ONELF_ARGV0` | Original argv[0] | `myapp` |
| `ONELF_EXEC` | Path to the packed binary | `/home/alice/bin/myapp.onelf` |
| `ONELF_ENTRYPOINT` | Active entrypoint name | `myapp-daemon` |
| `ONELF_ENTRYPOINT_EXPORTED` | The same name, so a launched package ignores it | `myapp-daemon` |
| `ONELF_LAUNCH_DIR` | Caller's original cwd | `/home/alice/project` |
| `ONELF_LIB_PATH` | The bundle's library directories, kept for older scripts (see below) | `/run/user/1000/onelf-myapp-ab12cd34/lib` |

## Library paths

The runtime does not set `LD_LIBRARY_PATH` for the app. A value the
caller set reaches the app unchanged, and so does its absence, and a
host program the app runs (a shell, `git`, `xdg-open`) sees the same
thing. The variable would otherwise carry the bundle's libraries into
every one of those programs.

Bundled binaries find their libraries through the `$ORIGIN` run path
`bundle-libs` wrote into them, which names the bundle's library
directory, `lib/` by default. Libraries from outside the bundle are
shown in that same directory as symlinks, so the run path finds them
too:

- the host libraries the resolver chose for this launch, and those a
  fetched [GL build](./sysroot#pinning-a-gl-build-for-hosts-without-one)
  supplies;
- the libraries of the package's
  [shared sets](./sysroot#sharing-a-dependency-set),
  including those in subdirectories of a set's `lib/`.

Only names the bundle does not carry are added. How the links come to
exist depends on the [mode](./execution-modes):

| Mode | The links are |
|------|---------------|
| `fuse` | entries the mount serves, decided by the launch before the app starts |
| `tmpfs`, `rundir` | symlinks in the launch's own tree |
| `cache` | symlinks in the shared cache tree, made to match what each launch resolved |

A FUSE mount another instance is serving (see
[Instances share a mount](./execution-modes#instances-share-a-mount)) is
reused only when it shows the links this launch resolved. A launch that
resolved differently, because the host changed or `ONELF_NO_RESOLVER` was
set, says so and mounts the package for itself. In the
cache mode a launch under a different host state rewrites the links,
and an instance still running from that tree loads the new targets for
anything it opens afterwards.

The library directories recorded at pack time (`--lib-dir`, by default
every directory that holds a `.so`) still form a search path, used
only when the runtime invokes the dynamic linker itself:

```
<resolver link farm>:<bundle lib dirs>:<shared set lib dirs>:<previous LD_LIBRARY_PATH>
```

It is passed as `--library-path` on that one command line, so nothing
the app spawns sees it.

⚠️ One kind of binary still gets `LD_LIBRARY_PATH`: an ELF entrypoint
without an `$ORIGIN` run path that the kernel has to start. That is a
self-extracting binary, an executable `bundle-libs` could not give a
run path (no slot and no `patchelf`), or a binary packed without
`bundle-libs` that runs under the host's loader. Nothing else leads it
to its libraries, so it gets the bundle's library directories ahead of
the previous value, and the programs it runs inherit them.
`bundle-libs` reports such executables at pack time.

`ONELF_LIB_PATH` names the bundle's library directories. Scripts used
to export it as `LD_LIBRARY_PATH` before running bundled binaries; that
is no longer needed, since those binaries find every library through
their own run path, and the variable is kept so such scripts go on
working.

The link farm holds the host libraries the runtime's resolver chose for
this launch: the driver stack the bundle does not carry, and any library
whose host copy is newer than the bundled one. See
[Bundling](./bundling#libraries-that-come-from-the-host) for how the
choice is made.

❌ Do not count on a host library directory being searched. No host
directory ever is: what the host supplies is chosen per library by the
resolver and linked by name, and a library the bundle lacks and the
resolver did not choose fails by name instead of being satisfied from
the host.

Each variable below is set only when the directory it names exists.
"A lib dir" means one of the bundle's library directories or, on a host
that received a
[GL build](./sysroot#pinning-a-gl-build-for-hosts-without-one), one of
the build's. Whether a value the caller set wins depends on the
variable; see [Which values the caller can override](#which-values-the-caller-can-override).

- `LIBGL_DRIVERS_PATH` and `LIBVA_DRIVERS_PATH` to every `dri/` in a
  lib dir
- `GBM_BACKENDS_PATH` to every `gbm/` in a lib dir
- `GCONV_PATH` to every `gconv/` in a bundle lib dir, the bundled
  glibc's charset converters
- `VK_DRIVER_FILES`, only when the loader's own search would not do;
  see [Vulkan drivers](#vulkan-drivers)
- `__EGL_VENDOR_LIBRARY_DIRS` to the package's, the GL build's and the
  host's `glvnd/egl_vendor.d` directories
- `LIBDRM_IDS_PATH` and `DRIRC_CONFIGDIR` for `share/libdrm/` and
  `share/drirc.d/`, from the package or else the GL build
- `LIBDECOR_PLUGIN_DIR` for `share/libdecor/plugins-1/`
- `XKB_CONFIG_ROOT` for `share/X11/xkb/`
- `GDK_PIXBUF_MODULE_FILE` and `GTK_IM_MODULE_FILE` for the
  `loaders.cache` under `lib/gdk-pixbuf-2.0/*/` and the
  `immodules.cache` under `lib/gtk-3.0/*/`, rewritten with absolute
  module paths
- `GIO_MODULE_DIR` for `lib/gio/modules/`, which replaces GLib's default
  module directory so the host's modules never load into the bundled
  GLib. A set's modules go on `GIO_EXTRA_MODULES`. A package without GIO
  modules leaves the default alone, so the host's dconf settings backend
  and gvfs keep working, and `bundle-libs` warns when it bundles GIO
  without them.
- `GTK_PATH` for `lib/gtk-3.0/`, where GTK 3 finds its modules, theme
  engines and print backends
- `FONTCONFIG_FILE` to a generated config that includes the host's
  `/etc/fonts/fonts.conf`, or the caller's own `FONTCONFIG_FILE` in its
  place, and adds the package's `share/fonts/`, so bundled fonts render
  and host fonts stay available

Plugin directories other libraries load from are found the same way,
in every library directory of the package and of its
[shared sets](./sysroot#sharing-a-dependency-set), so both `lib/` and
a multiarch `lib/x86_64-linux-gnu/` count:

- `GST_PLUGIN_SYSTEM_PATH_1_0` and `GST_PLUGIN_SYSTEM_PATH` for
  `gstreamer-1.0/`, and `GST_PLUGIN_SCANNER_1_0` and
  `GST_PLUGIN_SCANNER` for the bundled `gst-plugin-scanner`, looked for
  in that directory, in Debian's `gstreamer1.0/gstreamer-1.0/` and in
  the package's `libexec/gstreamer-1.0/`
- `PIPEWIRE_MODULE_DIR` for `pipewire-0.3/` and `SPA_PLUGIN_DIR` for
  `spa-0.2/`
- `ALSA_PLUGIN_DIR` for `alsa-lib/`, the first one found
- `GI_TYPELIB_PATH` for `girepository-1.0/`
- `LIBHEIF_PLUGIN_PATH` for `libheif/plugins/`, or `libheif/` itself
  where it has no `plugins/`
- `BABL_PATH` for `babl-0.1/` and `GEGL_PATH` for `gegl-0.4/`
- `TCL_LIBRARY` and `TK_LIBRARY` for a `tcl<version>/` holding
  `init.tcl` and a `tk<version>/` holding `tk.tcl`, below a library
  directory or under Debian's `share/tcltk/`

Data a library reads from a fixed path is looked for in the package's
root, then in the sets':

- `ALSA_CONFIG_DIR` for `share/alsa/` and `ALSA_CONFIG_PATH` for
  `share/alsa/alsa.conf`
- `XLOCALEDIR` for `share/X11/locale/`, the X11 compose and locale
  tables
- `MAGIC` for `share/file/misc/magic.mgc` or `share/misc/magic.mgc`,
  libmagic's database
- `TEXTDOMAINDIR` for `share/locale/`, where `gettext.sh` scripts look
  up their translations
- `TERMINFO_DIRS` gets `share/terminfo/` and `lib/terminfo/` added at
  the end. The caller's list stays in front, or when there is none the
  host's `/etc/terminfo`, `/lib/terminfo`, `/usr/share/terminfo` and
  `/usr/lib/terminfo`, so a terminal the host knows keeps the host's
  description and the package only fills in for a host without one.

`LOCPATH` names every `locale/` in a bundle lib dir that holds the
`C.UTF-8` locale `bundle-libs` carries. It is set only when the host has
no `/usr/lib/locale/locale-archive` and the caller did not set it,
because glibc stops reading the archive once `LOCPATH` is set, and a
host with an archive keeps every locale it has that way. On a host
without one, a musl host or a minimal container, the bundled glibc can
then set `C.UTF-8`, and it still looks in the host's `/usr/lib/locale`
after the package's.

`SSL_CERT_FILE` and `CURL_CA_BUNDLE` name the package's CA bundle, at
one of the paths distributions use such as
`etc/ssl/certs/ca-certificates.crt` or `etc/pki/tls/certs/ca-bundle.crt`,
only when the host has no bundle at any of those paths and the caller
set neither variable. A bundled TLS library looks where its packaging
distribution keeps the file, so a host that keeps it there or anywhere
else usual is left alone, and a minimal container without one can still
verify a connection.

GSettings schemas need no variable of their own: GLib finds them under
`glib-2.0/schemas` in every `XDG_DATA_DIRS` entry, and the package's
`share/` is one of those (see below).

The GLib, GTK, Qt and font entries also look in the package's
[shared sets](./sysroot#sharing-a-dependency-set).

✅ `QT_PLUGIN_PATH` names every `lib/qt6/plugins` and `lib/qt5/plugins`
the package and its [shared sets](./sysroot#sharing-a-dependency-set)
carry.

⚠️ The moment a set moves Qt elsewhere, plugin discovery breaks and the
abort prints nothing at all. A relocatable Qt finds its plugins relative
to the executable, which holds while the toolkit sits in the package and
stops holding when it does not. Qt then finds no platform plugin and
aborts, and it logs to the journal rather than stderr.

Finally, the package's own `share/`, then the GL build's and the sets',
are prepended to `XDG_DATA_DIRS`, so GLib/GTK discover bundled
GSettings schemas and icon themes. An unset `XDG_DATA_DIRS` is treated
as the XDG default, `/usr/local/share:/usr/share`.

### Vulkan drivers

The Vulkan loader finds the package's ICD files without help: the
package's `share/`, a GL build's and the sets' are on `XDG_DATA_DIRS`,
which the loader searches along with the host's `/etc/vulkan/icd.d`,
`/usr/local/share` and the rest of its usual places. The runtime leaves
the loader to that search when it is the host's, which is the case when
the resolver chose the host's `libvulkan.so.1` or the package carries
none.

It steps in with `VK_DRIVER_FILES` in two cases, listing the ICD files of
the package, a GL build and the sets, then those of the loader's usual
places in its order, `/usr/share/vulkan/icd.d` and
`/run/opengl-driver/share/vulkan/icd.d`:

- The package and the host both describe one driver, the same driver
  library named in an ICD file on each side. The loader would open it
  twice and show every device twice, so only one side's file stays: the
  package's when its own copy of the driver is the one that loads, and
  the host's when the resolver chose the host's copy or the package
  carries no copy. A host's 32-bit and 64-bit files for one driver are
  both kept.
- The package carries its own `libvulkan.so.1` and the resolver kept it.
  It was built for another system and may search other directories than
  this host uses, so it always gets the list.

❌ The runtime never sets `VK_ADD_DRIVER_FILES`. The package's files are
already on the loader's search through `XDG_DATA_DIRS`, and adding them
again would list them twice on the loaders distributions ship, which do
not drop a file found by two routes.

A caller's `VK_DRIVER_FILES`, `VK_ICD_FILENAMES` or `VK_ADD_DRIVER_FILES`
stops all of this, and the caller's choice reaches the loader untouched.

### Which values the caller can override

A variable naming modules that a bundled library loads into the app
takes the package's value whenever the package carries the component,
replacing one the caller set. Those modules were built against the
package's copy of the library, and a host value names modules built
against the host's, which the bundled library cannot load or loads to
crash. These are `QT_PLUGIN_PATH`, `GIO_MODULE_DIR`, `GIO_EXTRA_MODULES`,
`GDK_PIXBUF_MODULE_FILE`, `GTK_IM_MODULE_FILE`, `GTK_PATH`,
`GCONV_PATH`, `LIBDECOR_PLUGIN_DIR`, `FONTCONFIG_FILE`, the GStreamer
plugin path and scanner variables, `PIPEWIRE_MODULE_DIR`,
`SPA_PLUGIN_DIR`, `ALSA_PLUGIN_DIR`, `GI_TYPELIB_PATH`,
`LIBHEIF_PLUGIN_PATH`, `BABL_PATH`, `GEGL_PATH`, `TCL_LIBRARY` and
`TK_LIBRARY`. Typelibs and Tcl's scripts are not modules, but they are
written for one version of the library that reads them, which makes
them the package's in the same way. A caller's
`FONTCONFIG_FILE` is not lost: the generated config includes it where it
would otherwise include the host's `fonts.conf`.

The driver and data variables keep a value the caller set, because
picking a driver, a driver configuration or a keymap is what a user sets
them for, and the drivers they name are the host's by design. These are
`LIBGL_DRIVERS_PATH`, `LIBVA_DRIVERS_PATH`, `GBM_BACKENDS_PATH`,
`__EGL_VENDOR_LIBRARY_DIRS`, `VK_DRIVER_FILES` (and `VK_ICD_FILENAMES`
and `VK_ADD_DRIVER_FILES`, which stop the runtime from building
`VK_DRIVER_FILES` at all),
`DRIRC_CONFIGDIR`, `LIBDRM_IDS_PATH` and `XKB_CONFIG_ROOT`. So do
the data variables a user sets to choose other data: `ALSA_CONFIG_DIR`,
`ALSA_CONFIG_PATH`, `XLOCALEDIR`, `MAGIC` and `TEXTDOMAINDIR`.
`TERMINFO_DIRS` is extended rather than replaced, as described above.
`SSL_CERT_FILE` and `CURL_CA_BUNDLE` are set only when the caller set
neither.
`LOCPATH` is set only when the caller did not set it.

Any of these variables that arrives pointing into another onelf package
is dropped before the launch, whichever kind it is. Such a value is that
package's, and a program it started passed it on. A path counts as
another package's when a directory above it holds `.onelf/`, or when it
is a file a runtime generated in the private runtime directory, such as
a rewritten module cache or font config. The package's own value then
takes its place, or the variable stays unset.

## Data paths compiled into a library

The problem: the variables above cover the toolkits, but an application
that hardcodes an absolute data path cannot be helped. The path names the
host's filesystem, so on a machine that happens to have the same package
installed the app silently reads the host's copy, and on one that does
not it fails. The bundled copy goes unused either way.

The symptom is a package that works on the machine that built it and
fails in a container with a message about a missing file under `/usr`.
`libqalculate` reading `/usr/share/qalculate` is one example.
`bundle-libs` reports the known components whose compiled-in
directory the bundle carries no copy of; see
[Bundling](./bundling#plugin-and-data-paths-compiled-into-a-library).

- ✅ Test in a container that does not have the app installed.
- ✅ Where the app offers an environment variable for the directory, set
  it from the recipe:

```toml
[env]
QALCULATE_DEFINITIONS_DIR = "${ONELF_DIR}/share/qalculate"
```

An app with no such variable needs `[paths]`, below.

## Making the bundle answer for a compiled-in path

The problem: the application offers no variable for the path it opens.
✅ Name the path in the recipe and the runtime makes the bundle's own
directory answer for it:

```toml
[paths]
"/usr/share/galculator" = "share/galculator"
```

The key is the absolute path the application opens, the value is the
directory in the bundle that should be found there. The runtime enters a
private mount namespace before executing the entrypoint, so the change
is visible to the application and the processes it starts, and to
nothing else on the machine. Where the path already exists it is bound
over; where it does not, the bundle's directory is laid over the
parent so that one name appears without hiding what the parent holds.

Two cases fall back to the old behaviour, each with a message on the
terminal rather than a silent difference:

- A host that does not allow an ordinary user to make a user namespace.
- A package built with `needs-setuid`, or a launch with
  `ONELF_FUSE_NO_NAMESPACE` set. Both ask to stay in the host's user
  namespace, because a setuid bit does nothing inside one onelf made,
  and the mounts have to go where the namespace goes.

Memfd mode does not apply `[paths]` at all, since no directory of the
package exists on disk there. See
[Execution Modes](./execution-modes#mode-1-memfd-fastest-most-limited).

☑️ Reach for this only when there is no environment variable to set.
A variable costs nothing and works everywhere; this needs a namespace.

## Custom environment variables

✅ Declare extra env vars in the recipe's `[env]` section.
`${ONELF_DIR}` in values expands to the package root at runtime, so
paths follow the running app:

```toml
[env]
PYTHONHOME = "${ONELF_DIR}/python"
MYAPP_PLUGIN_DIR = "${ONELF_DIR}/lib/myapp/plugins"
```

In a recipe value, `${VAR}` expands from the packer's environment at
build time and is kept as written when the packer does not have it.
Write `$${VAR}` to defer a reference to launch, where it expands
against the **live** environment, with POSIX `${VAR:-word}` defaults.
That is how a value prepends instead of replacing. Every package gets
`PATH=${ONELF_DIR}/bin:${PATH:-/usr/bin:/bin}` by default, so the
package's `bin/` is always on `PATH` (re-exec-safe), falling back to
`/usr/bin:/bin` when the inherited PATH is empty. A recipe that sets
`PATH` in `[env]` replaces the default.

Memfd mode has no package tree, so it skips every value that names
`${ONELF_DIR}`, the default `PATH` among them, and applies the rest.

See [Recipe File](./recipe#env) for details.

## Surviving a sandboxed re-exec

The problem: some apps re-exec themselves with a wiped environment
(Chromium and Electron zygotes, Steam, `bwrap`-based sandboxes). Anything
the runtime exported, `[env]` included, is lost across that
`clearenv()` + `execve()`, because the runtime is no longer in the loop
on the re-exec.

❌ Do not look for the fix in the environment. onelf moves the guarantee
into the ELF itself:

- **Libraries**: `bundle-libs` bakes an `$ORIGIN` `DT_RPATH` into every
  binary, the one entry that reaches `lib/` from the binary's own depth,
  so bundled libs resolve relative to its location on every exec. The
  links to host and set libraries live in that directory too.
  ⚠️ Executables that could not get one (no in-place slot and no
  `patchelf`, or self-extract binaries) are reported at pack time. They
  fall back to `LD_LIBRARY_PATH` and are not re-exec-safe.
- **`[env]` and `preload`**: a tiny freestanding `onelf-env`
  constructor is bundled into `lib/` and injected as a `DT_NEEDED` of
  every executable that runs under the bundled loader. Because `DT_NEEDED` lives in the ELF and is resolved
  via the `$ORIGIN` run path above, it loads on *every* exec; its
  constructor re-applies `.onelf/env` and `.onelf/preload` before
  `main()`, no matter how the app cleared the environment.

⚠️ The `onelf-env` injection requires `patchelf` at pack time (set
`ONELF_PATCHELF` to override its location) and a prebuilt `onelf-env`
blob for the target architecture. When either is missing, packing prints
a warning and `[env]` is applied only on the first launch (the runtime
layer), not after a re-exec.

## Programs the app starts

The problem: everything above is set for the app, and every process
inherits its parent's environment. Left alone, the browser `xdg-open`
starts would get the package's `bin/` first on its `PATH`, the
package's Qt plugins, and with a [portable home](./portable-dirs) a
fresh profile instead of the user's.

✅ A program outside the package gets the environment the app was
launched with. The `onelf-env` constructor that `bundle-libs` injects
into every executable that runs under the bundled loader, helpers kept
under `lib/` included, defines `execve`, `execv`, `execvp`, `execvpe`, `posix_spawn` and
`posix_spawnp`. Just before it runs the entrypoint, the runtime records
in `ONELF_HOST_ENV` the starting value of every variable it set, changed
or removed: `[env]`, the portable directories, the toolkit and driver
variables above, `PATH`, and its own `ONELF_*`. When the app starts a
program, the hook resolves it the way the call will, searching `PATH`
for the `p` variants and following symlinks, and then:

| Program | Starts with |
|---------|-------------|
| Under the package root or one of its [shared sets](./sysroot#sharing-a-dependency-set) | the environment it was given, unchanged |
| Anywhere else, and `xdg-open` or `gio-launch-desktop` wherever they are | every recorded variable back as the caller had it, set to its original value or unset, and none of onelf's private variables |

A recorded variable is put back only while it still holds what the
runtime set. ✅ A value the app changes on purpose, in its own
environment or in the one it hands to the call, reaches the program as
the app wrote it.

⚠️ A host shell is a host program. `sh -c '$ONELF_DIR/bin/tool'` sees
no `ONELF_DIR` and expands it to nothing. Expand the path in the app, or
start the bundled program directly. A bundled program started from a
host shell gets the host environment plus what its own `onelf-env`
constructor applies from `[env]`, but not the toolkit variables above.

❌ Not covered:

- `system()`, `popen()` and the `execl` family. glibc and musl implement
  them through internal entry points that no definition outside libc
  can replace, so the shell or program they start keeps the app's
  environment.
- `fexecve` and `execveat`, which name the program by descriptor.
- A statically linked executable, and any executable without the
  `onelf-env` constructor: a package packed without `bundle-libs`, one
  packed without `patchelf`, or a self-extracting binary. None of them
  has the hooks.
- `onelf run`, which makes no record, so the hooks pass every
  environment through.

When a program started one of these ways is another onelf package, its
runtime still starts clean: it applies the record it inherited before
anything else, as the hook would have.

## Set by the user

| Variable | Effect |
|----------|--------|
| `ONELF_MODE` | Force a specific [execution mode](./execution-modes) |
| `ONELF_CACHE` | `1` allows the persistent cache for this launch without forcing it |
| `ONELF_ENTRYPOINT` | Select an [entrypoint](./entrypoints) by name |
| `ONELF_GC_MAX_AGE` | Cache GC threshold in days (default 30, `0` disables) |
| `ONELF_FUSE_NO_NAMESPACE` | Force the `fusermount3` path even when user namespaces work |
| `ONELF_FUSE_NO_SHARE` | Mount the package for this launch alone rather than run from a running instance's mount |
| `ONELF_NO_RESOLVER` | Take nothing from the host for this launch, to tell whether a failure is the resolver's |

The full list is in the [environment variable reference](../reference/env-vars).

## Portable directory redirection

If files named `<binary>.home`, `<binary>.config`, `<binary>.share`,
`<binary>.cache`, or `<binary>.env` exist next to the packed binary, the
runtime redirects the corresponding XDG env vars at them. See
[Portable Directories](./portable-dirs) for details.
