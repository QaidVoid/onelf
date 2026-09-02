# How onelf Thinks

Every single-file packaging tool on Linux answers one question, whether
it says so or not: **where is the line between what the package ships
and what the host provides?** macOS and Windows answer it for you. The
operating system promises a stable set of libraries, everything above
that line goes in the app bundle, and the toolchain knows the layout.
Linux makes no such promise, so every tool has to draw the line itself,
and most of what goes wrong in portable packaging is that line being
drawn in the wrong place, or fuzzily.

This page explains where onelf draws it and the four ideas that follow
from that. Everything else in the docs is detail on one of them.

## What a package ships, and what the host provides

The kernel is the one thing every Linux machine has in common, so a
package could in principle ship everything else. Two things stop that
from being the right answer:

- **Some code has to match the machine.** GPU drivers are the big one.
  Mesa and NVIDIA's userspace are chosen by the hardware and the kernel
  module in front of them, so the copy that works is the one on the host,
  and it must load *into your process*, next to your bundled libraries.
  The same goes for the NSS modules glibc uses for user and DNS lookups.
- **Some things are protocols, not libraries.** Wayland, X11, D-Bus,
  PipeWire and the desktop portals are all reached over a socket. The
  client library can be bundled freely, because the boundary is a wire
  format the host's daemons speak, not an ABI they export.

So onelf's line sits low: **a bundle ships its own libc, its own
libstdc++, its own everything, and takes only the driver stack and NSS
from the host.** That is what lets a package built on one distribution
run on musl systems and on hosts with older or newer libraries alike.

The list of what the host provides is called the **platform line**. It is
a short text file of soname prefixes, and it is the same list at pack
time, where those libraries are left out of the bundle, and at run time,
where the runtime goes and gets them.

```
+------------------------------------------------------------------+
|  host                                                            |
|    kernel                                                        |
|    GPU userspace: Mesa or NVIDIA, libva, libcuda   (platform     |
|    NSS modules                                       line)       |
|    sockets: Wayland, X11, D-Bus, PipeWire, portals               |
+------------------------------------------------------------------+
|  bundle                                                          |
|    the app, its libraries, its plugins and data                  |
|    glibc, libstdc++, the loader                                  |
|    Wayland, X11, D-Bus, PipeWire client libraries                |
+------------------------------------------------------------------+
```

## The resolver decides one library at a time

Taking a driver from the host sounds simple until you notice what the
driver brings with it. The host's Mesa was built against the host's
libstdc++, libdrm, LLVM and glibc. Your bundle carries its own copies of
those. Whether the two sets can share a process depends on which side is
newer, and that differs per library: the host may have a newer libstdc++
and an older libdrm at the same time. No order of directories on a search
path can express that.

So at every launch the runtime compares. For each library present both
in the bundle and on the host, it reads the symbol versions each copy
defines and takes the superset. A newer build of a library only ever adds
versions, so the superset can stand in for the other copy. The loader and
libc are compared first, as a pair, and if the host's glibc is newer the
whole process runs under the host loader. The chosen host libraries are
placed over their bundled counterparts inside a private mount namespace,
so every process the app starts sees the same choice.

What the resolver may touch is bounded by the package's **host-libs
policy**: `never` takes nothing, `auto` considers the driver stack and
what it pulls in, `always` considers every bundled library. No host
directory is ever put on the search path, so a library the bundle lacks
and the resolver did not choose fails by name instead of being quietly
satisfied by whatever the host has.

Detail: [Bundling: libraries that come from the host](./bundling#libraries-that-come-from-the-host).

## Two ways to decide what goes in the bundle

Before any of that, something has to decide what the bundle contains.
onelf has two answers.

**The host scan** reads the ELF files in your AppDir, follows their
`DT_NEEDED` entries through your machine's loader cache, and copies what
it finds. It is quick and needs nothing but a binary, and it cannot see a
`dlopen` by computed name, a plugin directory, or a data file. Its
universe is whatever your distribution installed.

**A sysroot** is a root filesystem with a package database, pinned and
kept outside the AppDir. Every file in it belongs to a package and every
package declares what it depends on, so the bundle can be the *closure*
of your application's package: its dependencies, their dependencies, and
the plugins and data those packages own, with nothing from your machine.
Three plain-text files trim the closure: the platform line, a **policy**
of paths that never ship, and optionally a **trace** of what a test run
opened.

Both end in the same **verifier**: every `DT_NEEDED` of every bundled
ELF must resolve inside the bundle or sit on the platform line. From a
sysroot an omission is an error, because the universe was complete. From
a host scan it is a warning, because the universe was a guess.

Detail: [Bundling from a Sysroot](./sysroot).

## Provenance, and what the sysroot label is for

A bundle built from a sysroot records where it came from: the sysroot's
label and every package that contributed a file, with versions. `onelf
info` prints it. It is display only, and it exists so that "does this
bundle carry a vulnerable libpng" is a question with an answer.

The label is the `platform` key under `[sysroot]` in the recipe. It
defaults to the archive's file name and it is just a name for the sysroot
a bundle was built against. Nothing in the runtime depends on it today.
The intent is that sysroots published for others to build on carry a
stable label, so that other things built for the same sysroot can be
matched to a bundle later. Until such a published sysroot exists you can
leave it at its default.

Detail: [Inspecting Packages](./inspecting#provenance).

## How a package runs

A packed file is a small static runtime with the bundle appended. When
run, it tries a ladder of ways to expose the bundle and uses the first
the host supports. Every rung but the last leaves nothing behind.

```
memfd                 a static single binary, straight from memory
userns + FUSE         a private mount nobody else can see; lazy
fusermount3 + FUSE    the host's helper, when namespaces are denied
userns + tmpfs        extract into RAM inside a private namespace
runtime directory     extract into the per-user runtime dir
persistent cache      only when the publisher or the user asks
```

The persistent cache is the one rung that leaves an extraction on disk,
so the runtime never falls into it on its own. A package that cannot run
any other way says so and names the variable that would allow it.

Detail: [Execution Modes](./execution-modes).

## Glossary

| Term | Meaning |
|------|---------|
| AppDir | The directory tree you pack: `bin/`, `lib/`, `share/` and so on |
| bundle | The contents of a packed file, or the act of filling an AppDir |
| closure | A package plus everything it depends on, transitively |
| platform line | The sonames the host provides; left out at pack time, fetched from the host at run time |
| policy | Glob patterns for paths that never ship |
| trace | Paths a test run opened, used to prune what it did not |
| sysroot | A pinned root filesystem with a package database, the source for a closure |
| sysroot label | The `platform` name recorded in a bundle's provenance |
| provenance | The record of which sysroot and packages a bundle came from |
| resolver | The launch-time step that chooses each library from the bundle or the host |
| link farm | The directory of chosen host libraries the resolver puts on the search path |
| rung, mode | One way of exposing the bundle to the kernel; the ladder tries them in order |
