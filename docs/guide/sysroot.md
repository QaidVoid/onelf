# Bundling from a Sysroot

`bundle-libs` normally works out what to ship by reading the ELF files in
your AppDir and looking their dependencies up on your machine. That is a
guess in two ways: a `dlopen` by computed name, a plugin directory or a
data file never appears in `DT_NEEDED`, and whatever it does find is
whatever your distribution happens to install.

A sysroot replaces the guess with a record. It is a root filesystem with
a package database, where every file belongs to a package and every
package declares what it depends on. The bundle's contents become the
closure of your application's package, minus what you declare should stay
out. Nothing from the machine you pack on is consulted.

The rules this page encodes:

- ✅ Pin a sysroot when the bundle must not depend on your machine. The
  bundle is the closure of your application's package, and two builds
  from the same archive and recipe produce the same bytes.
- ✅ Pin a set without a sysroot when the AppDir is already assembled (an
  upstream release unpacked): the tree stands, and only the sets are
  recorded for the runtime to fetch. See
  [Sets without a sysroot](#sets-without-a-sysroot).
- ✅ Prune that closure with the three tiers: platform line for what the
  host supplies, policy for what never ships, trace and keep file for
  what only a run proves.
- ✅ Expect a sysroot build to stop on an unresolved `DT_NEEDED`. The
  universe was complete, so the omission is a policy or platform-line
  mistake you can fix.
- ⚠️ Keep the sysroot outside the AppDir. `onelf build` and
  `bundle-libs` refuse one inside it, because everything under the
  AppDir is packed.
- ⚠️ Name the drivers nothing depends on when you `pack-gl`, Vulkan ICDs
  and `libva` included. A build without `libva` leaves a video player
  without it on a host that has none.

## Getting a sysroot

✅ An Arch Linux lineage rootfs. Its package database is a few plain-text
files per package, and debloated builds of the heavy packages (Mesa,
LLVM, Qt, ffmpeg) already exist for it. Any archive of such a rootfs
works, `.tar` or `.tar.zst`:

```bash
onelf sysroot fetch https://example.com/platform-1.tar.zst ./sysroot
onelf sysroot info ./sysroot
```

`fetch` also takes a local path, and refuses a plain `http://` URL,
since a sysroot is executable code. With `--hash`, an archive whose
BLAKE3 does not match is not unpacked. Materializing needs no privileges:
ownership is not restored and setuid bits are dropped, since the bundle
never needs either, and an archive whose entries reach outside the
directory is refused. The directory appears only once the whole archive
is unpacked, so a failed fetch leaves nothing a later build could take
for a sysroot.

✅ The quickest way to make one yourself is a container: install the
application with `pacman` inside an `archlinux` image and export the
container's filesystem.

⚠️ Keep the sysroot outside the AppDir. `onelf build` and `bundle-libs`
refuse one inside it, because everything under the AppDir is packed.

## Declaring it in the recipe

```toml
[package]
command = "bin/myapp"

[sysroot]
path = "../sysroot"
archive = "../platform-1.tar.zst"   # materialized into path when absent
optional = ["mesa", "vulkan-radeon"]  # packages to add to the closure
platform-line = "../platform-line.txt"
policy = "../policy.txt"
trace = "../trace.txt"               # optional
```

✅ Use `optional` for packages the closure misses on its own: an optional
dependency of something bundled, or a package nothing depends on at all,
such as a Vulkan driver. Each enters with its own dependencies, and a
name no package in the sysroot provides stops the build.

`onelf build` then finds the package owning `usr/bin/myapp`, takes its
transitive dependencies, prunes them, and copies the result into the
AppDir with `usr/` flattened into the usual `bin/`, `lib/` and `share/`.

The normal `bundle-libs` steps follow: run paths, the bootstrap, the
loader scrub. Their dependency lookup never leaves the sysroot: its
library directories, the directories its `etc/ld.so.conf` names, and
the RPATH of each binary, read as paths inside the sysroot. A library a
binary needs that sits outside `lib/`, under a vendor's `opt/` install
say, is copied into `lib/`, because the RPATH the packer writes reaches
`lib/` and nothing else.

The same works from the command line:

```bash
onelf bundle-libs ./myapp --target bin/myapp --sysroot ../sysroot \
  --platform-line ../platform-line.txt --policy ../policy.txt
```

## The three tiers

A closure is complete by construction, which makes it too large. Three
files decide what stays out. Each is plain text with `#` comments, so it
can be published, diffed and shared.

**The platform line** names what the host supplies, as soname prefixes:

```
# the host's GPU stack
libGL.so
libEGL.so
libnvidia
libcuda.so
```

A library on the line is left out of the bundle and reported as
host-provided; at run time the resolver takes it from the host; see
[Bundling](./bundling#libraries-that-come-from-the-host).

- ❌ Expecting a closure that includes Mesa to ship Mesa. Without a
  platform line the GPU driver families are the line, so it ships
  without Mesa.
- ✅ To bundle Mesa, write a platform line that leaves it out of the
  list.
- ⚠️ A package whose top-level library is on the line is the host's
  package and stays out whole: Mesa's gallium and DRI drivers serve the
  `libGL` the host supplies and nothing in the bundle, and so does
  whatever only such packages depended on, LLVM when nothing else needs
  it. The report lists them under "Host packages".
- ✅ When a file of a host package's that something bundled does need is
  reported as left out, the verifier names the needer, so the choice is
  yours: put the library on the platform line, or leave the package out
  of the line.

**The policy** names what never ships, as globs over paths relative to
the sysroot root. `*` stays within one directory and `**` crosses them,
so `usr/lib/*.a` leaves the subdirectories of `usr/lib` alone:

```
usr/share/doc/**
usr/share/man/**
usr/include/**
usr/lib/*.a
```

- ✅ Cut a distribution's closure down with it. It carries plugins whose
  own dependencies were never installed, Qt modules and Python bindings
  for optional stacks, and this is how they are cut.
- ✅ It holds for dependencies too. A library the policy prunes is not
  copied back because something bundled needs it; the build reports it
  as `removed` in its completeness findings, and the verifier names the
  file that needed it, which is the next thing to prune.

A build records what it put into the AppDir in `.onelf/generated`, with
the hash of each file, and removes those files first the next time, so
a changed policy or closure takes effect on its own while anything you
added by hand stays. Before it writes anything, the build checks every
destination it is about to fill:

- ❌ A file you placed where the sysroot puts a different one. The
  build stops, names both hashes, and replaces nothing. Remove the
  file, or move it to a path the build does not write.
- ❌ A recorded file whose bytes changed since the build wrote it, or
  an entry from an older ledger that carries no hash and whose bytes
  are not what this build writes. Either stays as it is, and the build
  stops until you remove or relocate it.
- ❌ A destination beneath a directory of the AppDir that is a symlink.
  The write would land wherever the link points, so the build stops.

A framework data directory the build carries to another destination
holds only the files the closure and the pruning kept, so a pruned file
never comes back through it.

**A trace**, when you have one, lists the paths a test run opened, one per
line. A file survives when it was opened, when any file in its directory
was opened, or when some surviving object names it in `DT_NEEDED`,
transitively, links to their targets included. The directory rule is
what keeps a plugin loaded by name from vanishing because the test run
did not happen to load it; it does not apply to the flat `bin/` and
`lib/` directories, where every run touches something and it would
keep everything. A Python package is kept whole once the run touched
anything in it, since its modules are imported lazily.

- ❌ Skipping the trace because the recipe builds fine without one.
  Without a trace nothing is pruned this way.
- ⚠️ A trace only knows what the run opened. Some data is loaded lazily
  too, icons and scripts an application reads on demand, and no test
  run opens all of it. **A keep file** names globs the trace may not
  prune, in the policy's format:

```
usr/share/blender/**
```

To capture a trace, run the packed application under `ONELF_TRACE`
pointing at a file. The runtime records the sysroot path of every file
the application opens after launch, already in the form the trace tier
reads. What the runtime itself reads before handing over, such as the
resolver's pass over every bundled library, is not recorded:

```bash
ONELF_TRACE=trace.txt ./myapp.onelf
```

The file is appended, so several runs accumulate into one trace. Do the
things the application has to be able to do: open a document, render a
frame, click through the interface, exercise the plugins you ship. Then
name the file in the recipe and rebuild.

Recording happens on the FUSE execution path, which sees every open, so
it needs a host where FUSE is available. That is the default on desktop
Linux. Under any other mode nothing is recorded, so set `ONELF_MODE=fuse`
to make sure the run takes this path. Build the package once without a
trace, capture, then add the `trace` line and build again.

❌ `--trace-run` on a sysroot build. It runs the target against the
packing machine, which a sysroot build does not use, so `bundle-libs`
refuses the combination. Record a trace with `ONELF_TRACE` instead.

## The verifier

After bundling, every `DT_NEEDED` of every bundled ELF must resolve inside
the bundle or name something on the platform line. From a sysroot that is
an error:

```
error: bin/myapp needs libfoo.so.1, which is neither in the sysroot closure, on the platform line, nor in a shared set
```

- ✅ A sysroot build stops, and that is the point. The universe was
  complete, so the omission is a policy or platform-line mistake you can
  fix. A host scan through `onelf build` fails on it too. Only the
  low-level `onelf bundle-libs` keeps it a warning when it scans this
  machine, because there the universe was a guess. When the file the
  build needs is not
  missing but pruned, the error names the conflict:

```
error: bin/app needs libdep.so.1, which libdep holds but the policy or a trace removed; the conflict stops the build rather than dropping the object or restoring the file
```

  An object the build would have to drop for this is listed as an
  optional omission in the completeness findings instead, so the failure is the one the
  build cannot resolve on its own: the entrypoint, a file you placed by
  hand, and an explicit include are never dropped silently.
- ☑️ A dependency the database names but the sysroot does not hold is
  reported, not fatal. A debloated rootfs drops metapackages on purpose.
- ☑️ A file the build copied whose need no package in the sysroot holds
  is dropped and listed as an optional omission, since nothing could load it: a
  plugin for a stack that was never installed, say. The entrypoint and
  files you placed by hand are never dropped. They stay findings.

## What the report tells you

```
Sysroot: ../sysroot
  Packages (4): app 1.0-1, glibc 2.44-1, libfixture 1.0-1, mesa 25.1-1
  Copied 212 files; left out: 3 on the platform line, 148 by policy, 0 by trace
  Host-provided: libGL.so.1, libEGL.so.1
```

The package list is the bundle's provenance, and it travels with the
package: `bundle-libs` writes it to `.onelf/provenance.toml` under the
`platform` label from the recipe, and `onelf info` prints it. ✅ Two builds
from the same archive and recipe produce the same bytes, so the list can
be checked against the archive later. A sysroot build pins every mtime
to the Unix epoch unless the recipe's `mtime` or `SOURCE_DATE_EPOCH`
names a time, since the time the AppDir was written says nothing about
the bundle.

## Sharing a dependency set

A toolkit the size of Qt or GTK is most of a bundle, and every package
carries its own copy. A shared set is that copy made once: the closure
of the named sysroot packages, packed as a build and fetched by any
package that pins it, so ten Qt applications share one download and one
extraction on a machine.

- ❌ Consulting the host's toolkit. Unlike the GL build a set is never
  conditional on the host: the host's toolkit is not consulted, because
  no loader can tell whether it is the right version, and the
  application behaves the same everywhere.
- ✅ Build the set from the sysroot the application is built on, so the
  two agree on every version:

```bash
onelf sysroot pack-set ./qt-tree -o qt-6.7.onelf --name qt --sysroot ../sysroot \
  --package qt6-base --package qt6-declarative
Built set qt from ../sysroot: 3140 files from 41 packages
blake3 = "3f1c...a9e2"
```

The set holds everything those packages need at runtime, plugins and
data included, and leaves out glibc and the development and
documentation directories. Then name it in the recipe:

```toml
[sysroot.sets.qt]
packages = ["qt6-base", "qt6-declarative"]
url = "https://mirror.example.org/qt-6.7.onelf"
blake3 = "3f1c...a9e2"
```

The build leaves the closure of those packages out of the bundle, along
with whatever only they depended on, reports what it left to the set,
and accepts what the set provides when it verifies the bundle. The
package records each set's URL and hash in `.onelf/sets`, which `onelf
info` prints. ✅ The hash is the trust story, as for the GL build.

Before anything is written, the build audits the sets it declares
against each other. Two sets that export one loader name with different
bytes cannot be selected together, because the entrypoint that names
both would have to load one of them twice, so such a pair stops the
build naming the entrypoint, the name and both pins:

```
error: provider audit failed:
  entrypoint 'app': libcollide.so.1 is exported with different bytes by sets alpha (blake3 f7400782b86f..(64 chars)) and beta (blake3 0ac0d405c79e..(64 chars)), so they cannot be selected together
```

Sets that export the same bytes under one name are one provider and
pass silently, and a set that exports no loader name contributes
nothing to the audit.

### At launch

Every set the invoked entrypoint selects is fetched into the shared
store under the cache root and verified against its hash before it is
placed there, so two packages naming the same set share one download.
An entrypoint that selects none fetches, mounts and extracts nothing,
whatever else the recipe declares.

By default the set is then served from that stored file: a detached FUSE
server mounts it at a mountpoint named after the build's hash, so nothing
is copied to disk and every package naming the set shares one mount, one
server and one block cache. The mount lives in the host mount namespace,
which is why sets are obtained before an execution mode is chosen; a mode
that unshares first would make the mount private to one launch. The
server takes the mount down once the last package using it exits, proven
by the mountpoint lock rather than guessed at. `ONELF_SET_MODE=extract`
asks for a copy through the ordinary package cache instead, and a host
that cannot mount falls back to that on its own.

- ⚠️ Mounting needs `fusermount3`, even on a host whose user namespaces
  work. The namespace mount the FUSE execution mode uses is private to
  the process that made it, so no other package could join it and the
  application, being a different process, would not see it either; and
  making one would put every launch with a set into a user namespace,
  which is what a `needs-setuid` package and a rootless podman or
  distrobox host ask the runtime not to do. Where the helper is missing
  the set is copied out instead, which costs disk but behaves the same.

Every shared object the set holds, in `lib/` and in any directory below
it, is shown in the bundle's library directory as a symlink, where the
bundled executables' run path finds it. A name the bundle kept stays the
bundle's, and the set supplies the rest. A set library that needs
another finds it the same way, through the executable's run path, which
the loader also searches for every library below it. Nothing goes on
`LD_LIBRARY_PATH`, so the programs the application runs never load the
set's libraries (see
[Environment](./environment#library-paths)). Its `share/` joins
`XDG_DATA_DIRS`, any Qt plugin directory it carries joins
`QT_PLUGIN_PATH`, and its libraries are compared against the host
exactly as bundled copies are.

⚠️ A toolkit that locates its own parts relative to the executable is the
one thing a set changes for the application, since the toolkit is no
longer beside it. Qt is the common case and the runtime handles it. An
application that hardcodes an absolute data path has the same problem
whether or not a set is involved; see
[Environment](./environment#data-paths-compiled-into-a-library).

Unlike the GL build, a set is not optional: the bundle carries none of
it, so a set that cannot be obtained ends the launch naming the set and
the reason, rather than failing later at the first missing symbol.
`ONELF_NO_PLATFORM_FETCH` and `ONELF_PLATFORM_STORE` apply to sets as
they do to the GL build.

### Store trust

✅ The pin is checked against the bytes, not the name. Before a stored
build is extracted, mounted, served or executed, the runtime hashes the
bytes it is about to consume against the package's full BLAKE3 pin. A
store file that carries the pinned name but holds different or
truncated bytes is not used: the launch fetches a permitted replacement
and verifies it the same way, or fails naming the set, the mismatch and
the pin:

```
onelf-rt: dependency set fix required by entrypoint app: <store>/fix/<pin>.onelf: hash mismatch (pinned bdbd...); fetching is disabled by ONELF_NO_PLATFORM_FETCH (fix would be fetched from file:///tmp/fix-set.onelf)
```

A valid hash says the bytes equal the pin and nothing more: it is not a
claim about who built them, which is why a package you do not trust
stays untrusted however well its pins verify.

✅ The store location travels as a hint, not a search path. An ordinary
launch exports its resolved store to the application and every process
it starts as `ONELF_PLATFORM_STORE`, so a bundled helper that fetches a
set looks where the launch did. The value is only ever an artifact
lookup location: no library directory is placed on it. A uid uses the
hinted store as its own only when that store's root directory belongs
to it. Root never does, and keeps its store under its own private base
(`/run/onelf` when `/run` is root's alone, otherwise a validated
`/tmp/onelf-0`), whatever `HOME` or `XDG_CACHE_HOME` say.

✅ Another user's store is import input, for root as for any other
uid. When the uid's own store lacks the pinned build, acquisition looks
in the hinted store before it fetches. The candidate is reached through the store without following
any symlink, must be a regular file, and is copied through a handle to
that same file into an exclusive scratch file in the uid's own store.
The copy's own bytes are verified against the pin and loaded as a
container before one rename publishes it, so a source changed or
replaced during the import yields either the pinned bytes or a refusal.
Nothing in the hinted store is written, renamed, chmod'd or touched.
`ONELF_NO_PLATFORM_FETCH` still permits this import, and any refusal
just moves on to the next permitted source.

✅ Old artifacts keep working. What a set provides and whether its
objects fit the package's libc are read out of the pinned build's
verified manifest and ELF objects at check time, so a set packed by an
older onelf needs no republishing when the checks grow stricter. A
wrong-architecture object or an unsatisfied symbol version in any
pinned build, old or new, fails before the application runs. A version
is checked against the copy of its library the launch supplies: the
package's own copy first, which keeps its precedence only while it
defines what the set needs, then a selected set's, then the bundled
libc for the libc family. A library none of them supplies, such as a
host GPU or media driver the set leaves out, is the host's and is not
checked. Two selected sets that export one name from their library
directories with different bytes fail the launch naming the name and
both pins, which also covers trees packed with `[bundle] skip`, where
the build audits nothing.

Resolver decisions are recorded per selection. An entrypoint that
selects no set never reuses the host-library decision a toolkit
entrypoint recorded, while two entrypoints naming the same pins in a
different order share one.

### Execution views

The libraries a launch shows, host choices and selected sets included,
reach it through an execution view: a private tree identified by
everything that changes what it shows or restores, the package, the
active set pins, the host-library decision, the entrypoint, the
consuming uid and the per-launch environment record among them. Views
live under the cache root, `views/view-<key>/`, built in private
staging and published in one atomic step, and a launch holds a lease on
its view for as long as it or any descendant runs.

Two launches of one package whose entrypoints select different sets
resolve different identities, so each gets its own view: the CLI sees
no toolkit links and cannot remove the links a running GUI is using,
and the GUI keeps loading its plugins while the CLI runs. Selections
that differ only in the order their lists were written share one view.
A change to any identity input, the host's resolver state included,
prevents reuse; `onelf cache gc` and `onelf cache clear` reclaim views
once no launch holds a lease. How a lease survives a daemonizing
application, and what becomes of a launch that is killed, is the
lifetime guardian's story; see [The Lifetime Guardian](./guardian).
See also
[Developing Packages](./developing#prepared-development-views) for the
development form of the same layout.

## Sets without a sysroot

A set can be pinned with no `path` in `[sysroot]` at all. That is the
shape for an AppDir you assembled yourself, an upstream release tarball
unpacked into place: there is no package database to take a closure
from, so the section takes nothing from one, and only records the sets
for the runtime to fetch at launch.

```toml
[package]
command = "bin/myapp"

[sysroot.sets.gtk]
packages = ["gtk3"]
url = "https://mirror.example.org/gtk-3.24.onelf"
blake3 = "3f1c...a9e2"
```

- ✅ Use it when the application's tree holds what it ships and only
  the shared toolkit is to come from a set. Bundling is otherwise
  unchanged: `bundle-libs` runs its ordinary host scan and copies in
  what the tree needs from the packer's machine, and the build writes
  the set records under `.onelf/`.
- ✅ Add `[bundle] skip = true` to pack the tree exactly as it stands:
  nothing is scanned, added or verified, and the set records are the
  only files the build writes.
- ❌ Expecting `archive`, `optional`, `platform-line`, `policy`,
  `trace`, `keep`, `platform` or `hash` to work here. Each decides what
  a package database's closure holds, and there is none; naming one
  without `path` stops the recipe from loading. A `[sysroot]` with
  neither `path` nor at least one set (nor a GL build pin) is refused
  too, since it has nothing to do.
- ✅ `platform-url` and `platform-hash` keep working without `path`:
  they name a GL build whole, which needs no database.

The verifier still audits the tree, and reads each set's pinned build
to see what it ships. A build is taken from its `file://` URL, or from
the platform store the runtime fetches into (`ONELF_PLATFORM_STORE`
when the packer's uid owns it, otherwise `platform/` under the cache
root); an `https://` build not
there yet is fetched into that store first, and kept only when its
BLAKE3 is the pinned one, so a wrong pin fails here rather than at a
user's launch. A soname a set's build ships is reported as that set's:

```
  Provided by set gtk: libgtk-3.so.0, libgdk-3.so.0
```

- ✅ Everything no set ships is reported as unresolved, so a library the
  tree truly lacks is not hidden behind a set that does not carry it.
- ⚠️ A set whose build cannot be had, offline or with a wrong pin, is
  reported with the reason, and claims nothing: what it would have
  supplied is listed with the missing libraries.

The package records the sets in `.onelf/sets`, the same file a sysroot
build writes, and `onelf info` prints them. A recipe that writes any
explicit `sets` list also carries the exact per-entrypoint selections
(see
[Selecting dependency sets](./recipe#selecting-dependency-sets)); one
that omits every list packs no selection record, which the runtime
reads as every declared set. A build made this way pins every entry's
mtime to 0 unless `[package].mtime` or `SOURCE_DATE_EPOCH` names a
time, as any `[sysroot]` build does.

## Pinning a GL build for hosts without one

The platform line says the host provides the GPU stack, so the bundle
carries none. Most hosts do. A minimal container, a headless CI runner
or an image built without Mesa does not, and there a GL application
would fail to load.

✅ A sysroot can name a build to fetch in that case, in
`etc/onelf/platform.toml`:

```toml
label = "platform-1"

[gl]
url = "https://example.com/platform-1/gl.onelf"
blake3 = "3f1c...a9e2"   # 64 hex characters
```

Every package built on the sysroot records those three values in
`.onelf/platform`, and `onelf info` prints them. The recipe can override
the URL and the hash:

```toml
[sysroot]
path = "../sysroot"
platform-url = "https://mirror.example.org/platform-1/gl.onelf"
platform-hash = "3f1c...a9e2"
```

✅ A mirror that serves a different file fails the hash check, so an
override can move the download but never change what is downloaded.

### Making the build

A GL build is an onelf package holding a tree with `lib/` (Mesa and its
drivers under `lib/dri`), `share/vulkan/icd.d` and
`share/glvnd/egl_vendor.d`. The sysroot is the source. Name the
packages, and `pack-gl` takes their closure, keeps the shared objects
and the driver description directories, leaves glibc to the package
that uses the build, and packs the result:

```bash
onelf sysroot pack-gl ./gl-tree -o gl.onelf --sysroot ../sysroot \
  --package mesa --package vulkan-radeon --package vulkan-icd-loader \
  --package libva
Built from ../sysroot: 812 files from libdrm 2.4.134-1, libglvnd 1.7.0-3, ...
blake3 = "3f1c...a9e2"
```

- ⚠️ Name the drivers nothing depends on, Vulkan ICDs in particular, and
  every family the platform line names that your applications use. A
  build without `libva` leaves a video player without it on a host that
  has none.

Without `--sysroot`, `pack-gl` packs a tree you built by
hand; either way it refuses a tree carrying glibc, then runs the
verifier over it. A library in `lib/` that needs anything apart from
glibc and the driver families the build exists to provide has to find
it inside, or the build stops naming what is missing, so the package
that provides it can be added. An object elsewhere with an unmet need, a
binding for a stack outside the closure, is dropped and listed, and so
is a library whose need the sysroot holds nowhere, since no build could
load it. It prints the hash to put in `platform.toml`.

✅ The hash is the whole trust story. The package that carries it is
already the thing you distribute, so whoever can alter the hash can
alter the package, and no key is needed to say who built the GL build.

### At launch

The runtime fetches only when all four hold: the host-library policy is
`auto` or `always`, the bundle carries no GL stack, the host has none,
and the package carries a pin. A package that bundles Mesa never
fetches, and neither does one on a host with a working driver.

☑️ Without a pin, the launch warns only when the bundle's libraries or
the binary it runs link a GL entry point the host lacks, since that
launch fails without one. A toolkit that opens GL by name, as GTK's
libepoxy and SDL do, does so only when the application draws with GL,
so a package that links none runs quietly on a host without a stack.

The file lands in `<cache root>/platform/<label>/<hash>.onelf` once its
hash matches. A mismatch or a broken download leaves nothing behind. It
is then extracted through the package cache and its libraries are
indexed ahead of the host's, so two packages pinning the same build
share one download and one extraction. The name is the build's own hash,
so a label whose pin later moves to a new build keeps both rather than
overwriting the old one and forcing a refetch. The build's
directories are never searched as a whole. Its libraries reach the
application through the link farm by name, the ones the drivers open
by name (glvnd vendors, gallium, ICDs) included, and are shown in the
bundle's library directory as the host's are. `onelf cache list` shows the
store and `onelf cache gc` collects a build no package has used past
the age threshold.

- ⚠️ Anything that prevents a fetch, including a missing pin, is a
  warning naming the reason, and the application is launched anyway.
- ☑️ A `file://` pin works with the slim runtime as well. A pin over
  `https://` needs the runtime that carries the HTTPS client, which
  `onelf pack` picks when the AppDir carries such a pin. Three variables
  adjust the behaviour at launch: `ONELF_NO_PLATFORM_FETCH`,
  `ONELF_PLATFORM_URL` and `ONELF_PLATFORM_STORE`, listed under
  [Environment Variables](../reference/env-vars).
