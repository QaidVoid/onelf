# Bundling Libraries

`onelf bundle-libs` walks the ELF files in an AppDir, resolves their
dependencies, and copies the shared libraries into `lib/`.

The rules this page encodes:

- ✅ Run `bundle-libs` on every AppDir before packing. It is idempotent;
  run it again whenever the tree changes.
- ❌ Never put a host directory on the library search path. Not with
  `LD_LIBRARY_PATH`, not with an `RPATH` pointing outside the tree. The
  resolver decides host access one library at a time; you cannot do
  better by hand.
- ✅ Reach for `--scan-dlopen` or `--trace-run` when the app loads
  plugins. `DT_NEEDED` alone does not see a `dlopen`.
- ❌ Do not bundle Mesa "to be safe". The driver that works is the
  user's; the host's driver stack is taken at launch unless you say
  otherwise.

## Basic usage

```bash
onelf bundle-libs ./myapp
```

With no flags, this:

1. Scans every ELF file under `./myapp/` for `DT_NEEDED` entries.
2. Resolves each soname via ldconfig (or the NixOS store, when detected).
3. Copies the resolved `.so` files into `./myapp/lib/`. With glibc come
   its charset converters, into `lib/gconv/`: the Unicode, Windows,
   ISO Latin, and common East Asian ones with the configuration that names
   them. `iconv` loads those by name from beside its own libc, and on a
   host with another glibc none would load, so a bundled app could not
   convert text at all. The runtime points `GCONV_PATH` at them. The
   `C.UTF-8` locale comes too, into `lib/locale/`, when the source
   system has it. It is not built into glibc, and a bundled glibc reads
   the host's locale archive, which a musl host or a minimal container
   does not have. A library resolved from a plugin directory of its own
   kind keeps that directory: the ALSA plugins a recipe force-includes
   land in `lib/alsa-lib/`, GStreamer's in `lib/gstreamer-1.0/`,
   PipeWire's in `lib/pipewire-0.3/` and `lib/spa-0.2/`, GIO's in
   `lib/gio/modules/`, and libheif's, babl's and GEGL's likewise. That
   is where their loaders look for them; a copy written flat would be
   dead weight, since nothing loads it from there. With the library
   comes the data it reads from a path compiled into it: libasound
   brings `share/alsa`, libxkbcommon `share/X11/xkb`, libX11
   `share/X11/locale`, and libmagic its compiled database
   `magic.mgc`, each copied from beside the resolved library. The
   carry is automatic and adds bulk on every matching bundle: a
   GUI toolkit's libxkbcommon brings roughly 3 MB of keymap data,
   libasound about 150 KB. A tree bundled by an older onelf is
   repaired on the next run: the missing data is carried then, and a
   plugin left flat in `lib/` moves into its subdirectory. Because of
   that repair work a re-run re-resolves every library instead of
   stopping at the first "all present" answer, which costs a few
   seconds more on a large tree and copies nothing when everything
   is current. On a host
   without the owning package,
   ALSA without `share/alsa` has no `alsa.conf` and cannot open any
   device at all, bundled plugins included. The carried `alsa.conf`
   also gets a hook entry inserted, loading the carried
   `alsa.conf.d/` right after the host's own snippet directories:
   the host's distro snippets then cannot steal the default output
   from the pulse default the package carries, while a deliberate
   `/etc/asound.conf` or `~/.asoundrc` still wins.
4. Rewrites `RPATH`/`RUNPATH` on every bundled ELF to the one `$ORIGIN`
   entry that reaches `lib/` from where the file sits: `$ORIGIN/../lib`
   for `bin/app`, `$ORIGIN/../../../lib` for `opt/tool/bin/app`. No entry
   ever resolves outside the tree, so the bundled binaries find their
   libs relative to their own location without `LD_LIBRARY_PATH` and
   without a host directory ever being searched. A `RUNPATH` is retagged
   as `RPATH`, which the loader passes down the dependency chain. For an
   executable without an existing slot, `bundle-libs` falls back to
   `patchelf --set-rpath` (when patchelf is in `PATH`, or named by
   `ONELF_PATCHELF`) to add a fresh entry.
5. Injects an AT_EXECFN bootstrap into each bundled executable. At
   runtime, the bootstrap reads `AT_EXECFN`, computes the bundled
   interpreter path relative to the binary's own location, and jumps
   into it. This keeps `/proc/self/exe` pointing at the real binary,
   which is important for Python's stdlib detection, Electron's ASAR
   locator, and Qt's plugin loader.
6. Scrubs `/usr/`, `/etc/`, `/nix/`, `/lib/`, and `/lib64/` strings
   inside the bundled glibc loader to `/XXX/`, so it won't pick up
   the host's `ld.so.preload`, `ld.so.cache`, or hardcoded fallback
   library dirs. A bundled musl loader has its default library path
   scrubbed the same way.

## Two ways to decide what goes in

✅ Host scan (this page) for a quick bundle of the app on your machine.
✅ A pinned sysroot when the result must not depend on your machine.

The walk above takes its universe from the machine you pack on: the
`ldconfig` cache, the Nix store, and the search paths you add. It cannot
see a `dlopen` by computed name, a plugin directory or a data file, and
what it finds is what your distribution installs. A pinned sysroot with
a package database is the other source: the bundle becomes the closure
of your application's package, pruned by what you declare, with nothing
taken from your machine. See [Bundling from a Sysroot](./sysroot).

Both sources end in the same verifier. Every `DT_NEEDED` of every bundled
ELF must resolve inside the bundle or name something the host provides.
How a finding is treated depends on where the tree came from:

- ✅ From a sysroot an unresolved soname is an **error**, since the
  universe was complete and the omission is a policy or platform-line
  mistake you can fix.
- ✅ From a host scan, `onelf build` and `onelf run --bundle` fail on
  an unresolved required dependency too, before anything is published
  or run. A need the host platform supplies (the libc family, the GPU
  driver stacks, and what `[bundle] exclude` leaves to the host) is
  reported and never fails the build. An auto-collected plugin whose
  dependency the scan cannot find is omitted and reported instead, and
  what the build requires (an entrypoint, an explicit include, a file
  you placed, a required helper or data group) is never omitted.
- ⚠️ The low-level `onelf bundle-libs` keeps the old behavior: from a
  host scan an unresolved soname stays a **warning** and nothing is
  omitted.
- ⚠️ A recipe with `[bundle] skip` runs no walk and no audit at all:
  the tree is packed **unchecked**, exactly as it stands. See
  [Unchecked trees](./recipe#unchecked-trees).

## Libraries that come from the host

The problem: some libraries must come from the host, most of the bundle
must not, and both land in the same process.

- ❌ `LD_LIBRARY_PATH` with host directories. It is inherited by every
  process the app spawns, so host libraries get loaded next to the
  bundled libc and break in ways that are miserable to trace. Never.
- ❌ Host directories before the bundle's on any search path. The host's
  libstdc++ loads beside a bundled glibc built against a different one.
- ❌ Host directories after the bundle's. The bundled copies win
  everywhere, and a host driver that must match the user's hardware
  fails to bind.
- ✅ Per-library resolution by the runtime's launch resolver. It
  compares versions and picks the superset, per soname.

A library the bundle does not provide is not a hard error at runtime,
but what the host may supply is decided one library at a time, at launch,
by the runtime's resolver. No host directory is ever placed on the search
path.

The problem the resolver exists for is a host GPU driver. Mesa and the
vendor drivers are `dlopen`'d by name and have to match the user's
hardware, so they come from the host, and they bring their own
dependencies with them: the host's libstdc++, libdrm, LLVM and, at the
bottom, the host's glibc. Whether those can share a process with the
bundled copies depends on which side is newer, and that differs per
library. Neither blanket order above is right for every library, which
is why comparing per soname is the design.

For each soname present both in the bundle and on the host, the runtime
reads the versions each copy defines and picks the superset. Under
symbol versioning a newer build only ever adds versions,
so the winner can stand in for the loser. Equal sets keep the bundled
copy. Two copies that each define a version the other lacks cannot be
ordered; the bundled one is kept and the soname is reported on stderr:

```
onelf-rt: resolver: cannot order libfoo.so.1; keeping the bundled copy
```

The loader and libc are compared first, as a pair, because a libc only
runs under the loader it shipped with. When the host's glibc is newer
than the bundled one, the whole glibc family comes from the host and the
entrypoint runs under the host loader. Every bundled library still loads
from the bundle, since a newer libc satisfies whatever the older one did.

Host winners are placed over their bundled copies with a bind mount
inside the private mount namespace, or a symlink in a tree that belongs
to this launch alone, so every process the app starts sees the same
choice, including one that clears its environment and re-execs. A mode
that has neither, the cache and a `fusermount3` mount, keeps the bundled
glibc, since the bundled executables would still find their own libc
under the host loader. Host
libraries the bundle does not carry at all, the driver itself for one,
are shown as symlinks in the bundle's library directory, where the run
path `bundle-libs` wrote finds them. No `LD_LIBRARY_PATH` is set, so the
programs the app runs never see them (see
[Environment](./environment#library-paths)). A soname that is neither
bundled nor chosen fails by name:

```
error while loading shared libraries: libm.so.6: cannot open shared object file
```

rather than loading whatever the host has and crashing later, somewhere
else, on a machine you do not have.

The decision is recorded under the private runtime directory, keyed by
the package and a fingerprint of the host's loader cache and driver
directories, so a second launch on an unchanged host reuses it. Set
`ONELF_NO_RESOLVER=1` to launch with nothing from the host, which is the
quickest way to tell whether a failure is the resolver's doing.

### What the resolver may consider

| Verdict | Mode | Meaning |
|---------|------|---------|
| default when the bundle references a driver stack | `auto` | The driver stack's host closure: the driver families, what the host's Vulkan and EGL vendor files name, the host's GLX vendor libraries, what the DRI and VA-API drivers in its `dri` directories need, and everything they need |
| default otherwise | `never` | Nothing from the host |
| ✅ for apps that load a host library under a name they never mention | `always` | Every bundled soname is compared as well |

```bash
onelf pack app -o app.onelf --command bin/app --host-libs always
```

or as `host-libs` under `[package]` in a recipe.

Detection is deliberately generous, because a wrong "never" breaks an app
that works while a wrong "auto" only costs a comparison. It matches
driver sonames anywhere in the bundle's binaries, not just in
`DT_NEEDED`, since drivers are reached through `dlopen`. Reach for
`always` if your app loads a host library under a name it does not
mention, such as one built at runtime from parts.

`bundle-libs` reports what it did not bundle:

```
warning: 1 librar(ies) are not in the bundle:
  - libm.so.6 (needed by myapp)
```

How to read that warning:

- ✅ GL, DRI, Vulkan and NSS entries are expected. They are meant to
  come from the host, because they have to match the user's drivers and
  system configuration.
- ❌ Anything else means the library exists on your machine but did not
  make the bundle, and the app will rely on the host for it. Fix it by
  pointing `--search-path` at a directory holding the library and
  repacking.

A library reached only through `dlopen` will not appear in `DT_NEEDED`
at all, so `--scan-dlopen` is what finds those.

## Plugin and data paths compiled into a library

The problem: GStreamer, PipeWire, ALSA, libheif, Tcl, libX11 and
libmagic load plugins or read data from a directory built into them,
such as `/usr/lib/gstreamer-1.0` or `/usr/share/alsa`. The runtime
points each at the bundle's copy through a variable, but only when the
bundle carries that copy (see
[Environment](./environment#library-paths)). Without one, the library
reads the host's directory, whose plugins were built against the host's
libraries, or finds nothing on a host without the package.

`bundle-libs` looks through every bundled file for the paths of a fixed
list of such components and reports each one the bundle has no copy
for:

```
warning: 2 compiled-in plugin or data path(s) have no copy in the bundle:
  - lib/libasound.so.2 reads /usr/lib/alsa-lib (carry alsa-lib in a library directory and the runtime sets ALSA_PLUGIN_DIR)
  - lib/libasound.so.2 reads /usr/share/alsa (carry share/alsa and the runtime sets ALSA_CONFIG_DIR)
```

- ✅ Copy the directory into the bundle where the line says, and the
  warning goes away.
- ✅ Or force-include the plugins by soname
  (`include = ["libasound_module_pcm_pulse.so"]`): a resolved plugin
  keeps the directory it was found in (`lib/alsa-lib/`), which is the
  copy the line asks for, and the warning clears on the next run. The
  soname alone is not enough to find the file: give the walk a
  `search-paths` entry pointing at the host's plugin directory, such as
  `/usr/lib/alsa-lib`.
- ✅ For the data directories (`share/alsa` and the rest), nothing to
  do: a bundled library brings its own data from beside it, and the
  warning clears without a hand-placed copy. It stays when the build
  host has none to bring.
- ☑️ Leave it when the app never uses that part of the library, such as
  an app that links libasound but plays no sound.
- ⚠️ Only paths stored whole in a library are found. GLib, GTK,
  gdk-pixbuf and gobject-introspection join theirs at run time, and a
  compiler may build a short path from pieces, so no warning is no
  proof. The driver stack, which comes from the host by design, and
  directories the runtime extends rather than replaces, such as
  terminfo and locales, are not reported.

## Self-extracting binaries (Bun, etc.)

- ⚠️ Binaries that embed a payload break under naive post-processing.
  The common case is Bun-compiled apps. Before 1.3.12 Bun looks for the
  trailer `\n---- Bun! ----\n` at the end of the file. From 1.3.12 it
  keeps the payload in a `.bun` ELF section that it finds by parsing its
  own program headers, so a rewritten layout makes it crash.

`bundle-libs` detects both kinds automatically and:

- Skips bootstrap injection. The bootstrap appends bytes and repurposes
  a program header, which would clobber the trailer or the section
  lookup.
- Skips `patchelf --set-rpath` for binaries lacking a RUNPATH slot,
  and leaves them out of `--strip`. Both rewrite the file.

At runtime, onelf-rt has the kernel resolve the PT_INTERP of a binary
with the end-of-file trailer directly. Inside a private mount namespace
it bind-mounts the bundled linker over the binary's existing PT_INTERP
path. Without one, as in cache mode, it creates a short symlink in the
private runtime directory and patches the binary's PT_INTERP in place
to point at it. Either way, `/proc/self/exe` resolves to the binary
itself and Bun finds its embedded JS bundle.

☑️ `bundle-libs` warns about each such executable that is left without
an `$ORIGIN` entry. It finds its libraries only through the
`LD_LIBRARY_PATH` the runtime sets, which an app that clears its
environment and re-execs itself loses.

## Starting from a bare binary

```bash
onelf bundle-libs ./myapp --from-binary /usr/bin/myapp
```

Copies `/usr/bin/myapp` into `./myapp/bin/myapp`, then runs the normal flow.

## Detecting dlopen'd libs

Some libraries are loaded at runtime via `dlopen` and don't appear in
`DT_NEEDED`.

- ✅ `--scan-dlopen` when the app loads known plugins. It searches the
  binary strings for common candidates (GL, Wayland, Vulkan, X11,
  audio, DBus, and so on) and bundles any matches.

```bash
onelf bundle-libs ./myapp --scan-dlopen
```

✅ Extend the allow-list with a vendor soname the scan cannot know:

```bash
onelf bundle-libs ./myapp --scan-dlopen --dlopen libmyvendor.so.1
```

✅ When even the scan is not enough, run the application and let the
loader say what it loaded:

```bash
onelf bundle-libs ./myapp --target bin/myapp --trace-run --trace-seconds 5 --trace-arg --version
```

The target runs for up to the given time, from this machine's libraries,
and every shared object it loaded from outside the tree joins the bundle,
subject to the same exclusions as everything else. glibc's loader reports
this on its own through `LD_DEBUG`, per process, so helpers the app
spawns count too; a musl target is watched with `strace`, which has to
be installed. Only the tree's own programs count: a host program the app
starts, a shell or `xdg-open`, does not. The run gets a scratch `HOME`
and XDG directories, runs in its own process group, and the whole group
is stopped at the end; it keeps the display and the D-Bus session. The
GPU stack it maps (Mesa, libLLVM, vendor GL and Vulkan drivers) is left
to the host as on detection. A run that fails on its own is reported
with the end of its stderr. ⚠️ A GUI app needs a display for the run; an app that reads
its arguments needs `--trace-arg` to make it exercise the path you care
about. ❌ A trace does not apply to a sysroot build, which records a
trace with `ONELF_TRACE` instead.

## Framework auto-detection

`bundle-libs` scans the binaries for the frameworks they use, by
`DT_NEEDED` and by literal soname strings in their bytes, so a library
that is only `dlopen`'d at runtime (Blender loading `libwayland-cursor.so`
after checking `$XDG_SESSION_TYPE`, for example) is found without a
`DT_NEEDED` entry.

✅ Detection only flags a framework when it finds a properly versioned
soname such as `libEGL.so.1` or `libwayland-client.so.0`. This keeps Rust
binaries honest. Their string tables merge literals together without NUL
separators, so a real dlopen soname shows up glued to its neighbours like
`...eglWaitSynclibEGL.so.1libEGL.so...`. The version suffix is what tells a
genuine soname apart from prose like `"Library libwayland-client.so could
not be loaded."`.

The verdict per framework:

- ✅ Wayland, GTK: bundled. Client-side, they must match the bundle.
- ✅ GL, DRI, Vulkan: not bundled. The driver that works is the host's.
  `pack` sees the driver references under its default `--host-libs auto`
  and the launch resolver takes the stack from the host. The scan
  reports which it found:

```
  Frameworks: gl, vulkan detected; the host's drivers are used at launch.
  Pass --gl, --dri or --vulkan to bundle Mesa instead.
```

- ❌ Bundling Mesa "to be safe": for a host that has its own GL this
  means two copies of the driver stack, and the resolver then takes the
  host's wherever it is newer. ✅ Bundle Mesa only when the package
  must run where the host has no GL at all.

You can still force any of these explicitly:

```bash
onelf bundle-libs ./myapp --gl --vulkan --wayland --gtk
```

✅ Opt out of a framework that detection or an explicit flag would
otherwise pull in. This matters for a binary built with optional GUI
support that you only ship as a TUI. `amdgpu_top`, for example, links
the wgpu and wayland stacks even though its default mode is a terminal
UI:

```bash
onelf bundle-libs ./amdgpu_top --no-gl --no-wayland
```

The `--no-*` opt-out always wins over both auto-detection and the matching
`--*` flag. Auto-enabled and suppressed frameworks are both printed so you
know what was decided.

## Framework rules

Behind the detection sits a built-in rule catalog. Each supported
component (ALSA, GStreamer, PipeWire, GTK, Qt, xkb, fontconfig, gdk-pixbuf,
Tcl/Tk and the rest) has a rule naming the sonames that activate it and
the plugin directories, helper executables and data directories the
component needs. When the walk sees an activating soname, the rule's
groups are collected from the selected source, so a component's plugins
and data arrive with it instead of being chased one `--include` and one
hand-placed `share/` at a time. Each activated rule is reported, with
the soname that triggered it and how many groups it queued:

```
  Rules: 1 activated by soname prefix: xkb (libxkbcommon.so.0); 1 group(s) queued
```

- ✅ A required group the selected source cannot supply stops the build
  naming the rule, the group and the locations it looked in:

```
error: rule alsa group configuration requires directory data at share/alsa, but none of its declared locations exist in the selected source; ./myapp: absent
```

  The trailing list is the audit's evidence: each examined location and
  why it held nothing (`absent`, `left to the host package`, `pruned
  from the package`), a few at most, so the fix is visible without
  rerunning. A required helper executable is treated the same way and
  names the rule and the helper it missed. The host's copy is never
  pulled in to fill the gap.
- ⚠️ An optional group the source happens not to have is a warning, not
  a failure:

```
warning: alsa/plugins: no usable plugin group (1 location(s) unavailable, 0 candidate(s) skipped)
    ./myapp/lib: absent
```

  The component keeps working through the runtime mappings the rule
  also declares, and the report says what was skipped.
- ⚠️ Activation reads sonames, not behavior. A plugin loaded under a
  name the binary computes at run time still needs `--trace-run` (host
  scan) or a sysroot trace, like every other `dlopen` by computed name.
- ⚠️ The catalog is built in. There is no recipe or CLI surface for
  project-local rules or for disabling a built-in one yet.

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

✅ Do nothing special. bundle-libs handles the NixOS stub loader
(`/lib64/ld-linux-x86-64.so.2`, which exists but refuses to run foreign
binaries, printing `NixOS cannot run dynamically linked executables...`)
automatically:

- Any candidate loader whose canonical path contains `stub-ld` is
  rejected at every resolution tier, so a real glibc from the Nix
  store wins.
- A previously-bundled stub-ld accidentally copied into `lib/` is
  detected (by path or by the `NixOS cannot run` signature inside
  the file) and deleted on the next `bundle-libs` run.
- The bundled glibc loader has its `/nix/store/...` build-time paths
  scrubbed so it can't reach back to a store path that doesn't exist
  on another machine.

⚠️ If you see the stub-ld error anyway, your `lib/` predates the fix:
delete it and re-run `bundle-libs` with the current `onelf`.

## Cross-libc hygiene

❌ Mixing libc families in one bundle. When the target binary is musl
and the host is glibc (or vice versa), bundle-libs can end up copying
libraries built against the wrong libc. Those fail at runtime with
confusing "symbol not found" errors.

✅ `--strict-libc` refuses to bundle libraries whose `DT_NEEDED` points
at the wrong libc family, and instead lists them under "Not found":

```bash
onelf bundle-libs ./myapp --strict-libc
```

✅ Combine with `--search-path` pointing at the right-libc versions to
make the bundle clean.

## Excluding and including

```bash
onelf bundle-libs ./myapp --exclude libpthread,libdl
onelf bundle-libs ./myapp --include libsomething.so.1
```

`--exclude` skips libraries by prefix. `--include` forces a soname into
the resolution queue.

☑️ Some prefixes are always excluded: `libnss_` (apart from the name
service modules a bundled glibc needs), the vendor GPU and compute
runtimes `libcuda.so`, `libnvidia`, `libamdhip64.so` and
`libze_loader.so`, and `linux-vdso.so`.

## Stripping

```bash
onelf bundle-libs ./myapp --strip
```

Runs `strip --strip-unneeded` on each copied library. Saves disk space
(often 20-40 %) at the cost of debuggability.

## Dry run

✅ Check a bundle plan before committing to it:

```bash
onelf bundle-libs ./myapp --dry-run
```

Shows what would be bundled without copying anything.

## GPU / graphics helpers

The granular framework flags:

| Flag | What it bundles |
|------|-----------------|
| `--gl` | The glvnd dispatch libraries (`libGL`, `libGLX`, `libEGL`, `libGLESv2`, `libOpenGL`, `libGLdispatch`), Mesa's `libGLX_mesa`, `libEGL_mesa`, `libglapi`, `libgbm` and `libxatracker`, GBM backends, EGL vendor files, and Mesa's `drirc.d` and `libdrm` data |
| `--dri` | Mesa DRI drivers into `lib/dri` (filtered to your architecture) |
| `--vulkan` | Vulkan ICD files into `share/vulkan/icd.d` and the drivers they name (filtered to your architecture) |
| `--wayland` | `libwayland-client`, `-server`, `-cursor` and `-egl`, `libdecor-0` with its plugins, and `libxkbcommon` |
| `--gtk` | GSettings schemas, compiled into `share/glib-2.0/schemas` |

- ✅ `--wayland` and `--gtk` are enabled by detection; pass them to
  force-on when detection misses something.
- ☑️ `--gl`, `--dri` and `--vulkan` are opt-in, since the driver stack
  comes from the host unless you say otherwise.
- Each has a `--no-*` counterpart (`--no-gl`, `--no-dri`,
  `--no-vulkan`, `--no-wayland`, `--no-gtk`) that forces-off, overriding
  both detection and the matching `--*` flag.
- ☑️ None of them apply to a [sysroot](./sysroot) build. On the command
  line they are refused with `--sysroot`, and in a recipe with a
  `[sysroot]` they do nothing.
