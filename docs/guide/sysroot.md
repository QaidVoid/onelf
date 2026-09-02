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
optional = ["mesa", "pipewire"]      # optional dependencies to include
platform-line = "../platform-line.txt"
policy = "../policy.txt"
trace = "../trace.txt"               # optional
```

`onelf build` then finds the package owning `usr/bin/myapp`, takes its
transitive dependencies plus the optional ones you name, prunes them, and
copies the result into the AppDir with `usr/` flattened into the usual
`bin/`, `lib/` and `share/`. The normal `bundle-libs` steps follow: run
paths, the bootstrap, the loader scrub. Their dependency lookup is
restricted to the sysroot's own library directories.

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

**The policy** names what never ships, as globs over paths relative to
the sysroot root:

```
usr/share/doc/**
usr/share/man/**
usr/include/**
usr/lib/*.a
```

**A trace**, when you have one, lists the paths a test run opened, one per
line. A file survives when it was opened, when any file in its directory
was opened, or when some bundled object names it in `DT_NEEDED`. The
directory rule is what keeps a plugin loaded by name from vanishing
because the test run did not happen to load it. Without a trace nothing is
pruned this way.

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
`share/glvnd/egl_vendor.d`. Build the tree on the sysroot, then:

```bash
onelf sysroot pack-gl ./gl-tree -o gl.onelf
blake3 = "3f1c...a9e2"
```

The command runs the verifier over the tree first: everything it needs
apart from glibc, which the package that uses it carries, and the driver
families it exists to provide, has to be inside. It prints the hash to
put in `platform.toml`.

The hash is the whole trust story. The package that carries it is
already the thing you distribute, so whoever can alter the hash can
alter the package, and no key is needed to say who built the GL build.

### At launch

The runtime fetches only when all four hold: the host-library policy is
`auto` or `always`, the bundle carries no GL stack, the host has none,
and the package carries a pin. A package that bundles Mesa never
fetches, and neither does one on a host with a working driver.

The file lands in `<cache root>/platform/<label>/gl.onelf` once its
hash matches; a mismatch or a broken download leaves nothing behind. It
is then extracted through the package cache and its libraries are
indexed ahead of the host's, so two packages pinning the same label
share one download and one extraction. `onelf cache list` shows the
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
