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

## Getting a sysroot

A sysroot is an Arch Linux lineage rootfs, chosen because its package
database is a few plain-text files per package and because debloated
builds of the heavy packages (Mesa, LLVM, Qt, ffmpeg) already exist for
it. Any archive of such a rootfs works, `.tar` or `.tar.zst`:

```bash
onelf sysroot fetch https://example.com/platform-1.tar.zst ./sysroot
onelf sysroot info ./sysroot
```

`fetch` also takes a local path. Materializing needs no privileges:
ownership is not restored and setuid bits are dropped, since the bundle
never needs either, and an archive whose entries reach outside the
directory is refused.

The quickest way to make one yourself is a container: install the
application with `pacman` inside an `archlinux` image and export the
container's filesystem.

::: warning
Keep the sysroot outside the AppDir. `onelf build` refuses one inside it,
because everything under the AppDir is packed.
:::

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

`onelf build` then finds the package owning `usr/bin/myapp`, takes its
transitive dependencies, prunes them, and copies the result into the
AppDir with `usr/` flattened into the usual `bin/`, `lib/` and `share/`.
`optional` names packages to add: an optional dependency of something in
the closure, or a package nothing depends on at all, such as a Vulkan
driver. Each enters with its own dependencies.

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
host-provided. At run time the resolver takes it from the host; see
[Bundling](./bundling#libraries-that-come-from-the-host). Without a
platform line the GPU driver families are the line, so a closure that
includes Mesa ships without it. To bundle Mesa, write a platform line
that leaves it out of the list.

A package whose top-level library is on the line is the host's package,
and it stays out whole: Mesa's gallium and DRI drivers serve the
`libGL` the host supplies and nothing in the bundle. So does whatever
only such packages depended on, LLVM when nothing else needs it. The
report lists them under "Host packages". A file of theirs that
something bundled does need is reported as left out, and the verifier
names the needer, so the choice is yours: put the library on the
platform line, or leave the package out of the line.

**The policy** names what never ships, as globs over paths relative to
the sysroot root:

```
usr/share/doc/**
usr/share/man/**
usr/include/**
usr/lib/*.a
```

It holds for dependencies too. A library the policy prunes is not
copied back because something bundled needs it; the build reports it
under "Left out by policy" and the verifier names the file that needed
it, which is the next thing to prune. A distribution's closure carries
plugins whose own dependencies were never installed, Qt modules and
Python bindings for optional stacks, and this is how they are cut.
A build records what it put into the AppDir in `.onelf/generated` and
removes those files first the next time, so a changed policy or
closure takes effect on its own while anything you added by hand
stays.
**A trace**, when you have one, lists the paths a test run opened, one per
line. A file survives when it was opened, when any file in its directory
was opened, or when some surviving object names it in `DT_NEEDED`,
transitively, links to their targets included. The directory rule is
what keeps a plugin loaded by name from vanishing because the test run
did not happen to load it; it does not apply to the flat `bin/` and
`lib/` directories, where every run touches something and it would
keep everything. A Python package is kept whole once the run touched
anything in it, since its modules are imported lazily. Without a trace
nothing is pruned this way.

Some data is loaded lazily too, icons and scripts an application reads
on demand, and no test run opens all of it. **A keep file** names globs
the trace may not prune, in the policy's format:

```
usr/share/blender/**
```

Capturing a trace is a matter of running the application from the
package and recording what it opens. `strace -f -y -e trace=%file` on
a run in cache mode (`ONELF_MODE=cache ONELF_CACHE=1`) lists every
path under the extracted tree; map `lib/`, `bin/` and `share/` back
to `usr/` and the result is the trace. Run the paths you care about: a
render, a file import, a session in the interface.

## The verifier

After bundling, every `DT_NEEDED` of every bundled ELF must resolve inside
the bundle or name something on the platform line. From a sysroot that is
an error:

```
error: bin/myapp needs libfoo.so.1, which is neither in the sysroot closure nor on the platform line
```

The universe was complete, so the omission is a policy or platform-line
mistake you can fix. From a host scan the same finding stays a warning,
because there the universe was a guess.

A dependency the database names but the sysroot does not hold is
reported, not fatal. A debloated rootfs drops metapackages on purpose.

## What the report tells you

```
Sysroot: ../sysroot
  Packages (4): app 1.0-1, glibc 2.44-1, libfixture 1.0-1, mesa 25.1-1
  Copied 212 files; left out: 3 on the platform line, 148 by policy, 0 by trace
  Host-provided: libGL.so.1, libEGL.so.1
```

The package list is the bundle's provenance, and it travels with the
package: `bundle-libs` writes it to `.onelf/provenance.toml` under the
`platform` label from the recipe, and `onelf info` prints it. Two builds
from the same archive and recipe produce the same bytes, so the list can
be checked against the archive later.

## Pinning a GL build for hosts without one

The platform line says the host provides the GPU stack, so the bundle
carries none. Most hosts do. A minimal container, a headless CI runner
or an image built without Mesa does not, and there a GL application
would fail to load.

A sysroot can name a build to fetch in that case, in
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

A mirror that serves a different file fails the hash check, so an
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

Name the drivers nothing depends on, Vulkan ICDs in particular, and
every family the platform line names that your applications use: a
build without `libva` leaves a video player without it on a host that
has none. Without `--sysroot`, `pack-gl` packs a tree you built by
hand; either way it refuses a tree carrying glibc, then runs the
verifier over it, so everything it needs apart from glibc and the
driver families it exists to provide has to be inside. It prints the
hash to put in `platform.toml`.

The hash is the whole trust story. The package that carries it is
already the thing you distribute, so whoever can alter the hash can
alter the package, and no key is needed to say who built the GL build.

### At launch

The runtime fetches only when all four hold: the host-library policy is
`auto` or `always`, the bundle carries no GL stack, the host has none,
and the package carries a pin. A package that bundles Mesa never
fetches, and neither does one on a host with a working driver.

The file lands in `<cache root>/platform/<label>/<hash>.onelf` once its
hash matches, with the hash beside it so a label whose pin moves on to
a new build is fetched again; a mismatch or a broken download leaves
nothing behind. It is then extracted through the package cache and its
libraries are indexed ahead of the host's, so two packages pinning the
same label share one download and one extraction. The name is the build's own
hash, so a label whose pin later moves to a new build keeps both rather
than overwriting the old one and forcing a refetch. The build's
directories are never put on the search path, where every process the
application spawns would inherit them. Its libraries reach the
application through the link farm by name, the ones the drivers open
by name (glvnd vendors, gallium, ICDs) included. `onelf cache list` shows the
store and `onelf cache gc` collects a build no package has used past
the age threshold.

Anything that prevents a fetch, including a missing pin, is a warning
naming the reason, and the application is launched anyway.

A pin over `https://` needs the runtime that carries the HTTPS client,
and `onelf pack` picks it when the AppDir carries such a pin. A
`file://` pin works with the slim runtime as well. Three variables
adjust the behaviour at launch: `ONELF_NO_PLATFORM_FETCH`,
`ONELF_PLATFORM_URL` and `ONELF_PLATFORM_STORE`, listed under
[Environment Variables](../reference/env-vars).
