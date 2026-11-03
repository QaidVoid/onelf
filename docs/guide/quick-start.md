# Quick Start

Turn a Linux binary into a single portable file in under a minute. This
page assumes `onelf` is [installed](./installation) and that the binary
runs on your machine.

## The short version

1. **Scaffold** a recipe beside nothing but the binary itself:

   ```bash
   onelf init --binary /path/to/mybinary
   ```

   This writes an `onelf.toml` whose command is `bin/mybinary` and
   whose `[input]` section names the binary you gave it. On the next
   step the build copies it to `bin/mybinary` and packs the AppDir it
   assembles around the recipe: the recipe's directory is that AppDir,
   and nothing outside it is touched. An AppDir that already holds the
   binary needs no `[input]` section: a recipe whose only field is
   `command` builds the same way.

2. **Build** the package:

   ```bash
   onelf build
   ```

   One command does the whole journey: it stages the imported binary,
   reads the libraries the binary links (`DT_NEEDED`) and the
   commonly `dlopen`ed libraries named in its strings, copies every one
   it needs into `lib/`, and packs the result. Mesa and the other GPU
   drivers are left out on purpose: the packed app uses the host's. The
   executable lands next to the recipe, named after the command, here
   `mybinary.onelf`.

3. **Run** it:

   ```bash
   ./mybinary.onelf --any --args --you --like
   ```

   Arguments go straight to the app. Copy `mybinary.onelf` to another
   machine and it runs there too.

On a typical desktop the runtime mounts the package as a private FUSE
filesystem in a user namespace, `exec`s the target, and lets the kernel
clean everything up on exit. Hosts that deny user namespaces get one of
the fallbacks in [Execution Modes](./execution-modes).

## Without a recipe

The same two steps by hand, for a one-off:

```bash
onelf bundle-libs ./mypkg --from-binary /path/to/mybinary --scan-dlopen
onelf pack ./mypkg -o mybinary.onelf --command bin/mybinary
```

`bundle-libs` copies the binary to `./mypkg/bin/mybinary` and its
libraries into `./mypkg/lib/`; `pack` turns the directory into one
executable. `--command` is the path of the binary inside the AppDir.
The command line cannot do everything the recipe can, so anything
reused or committed to a repo belongs in an `onelf.toml`.

## If something is missing

A library the app loads by a computed name at run time is invisible to
the scan in step 2, and the packed app fails on another machine with
`cannot open shared object file`. Let the app show what it loads:

```bash
onelf bundle-libs ./mypkg --target bin/mybinary --trace-run
```

This runs the binary for a few seconds and bundles every library it
opened. Then pack again. [Bundling Libraries](./bundling) covers the
other ways to find and add libraries.

## Dev loop

Run the AppDir in place, without packing:

```bash
onelf run --bundle       # assemble from the recipe, then run
onelf run                # subsequent runs: reuse the prepared view
```

`onelf run` stages the tree once, uncompressed, into a private
per-identity view under the onelf cache and reuses it untouched until
something that changes what the launch shows or restores changes. See
[Developing Packages](./developing).

## Inspect

```bash
onelf info mybinary.onelf     # metadata, entrypoints, sizes
onelf list mybinary.onelf     # file tree
onelf verify mybinary.onelf   # BLAKE3 integrity check
./mybinary.onelf --onelf-doctor   # host, sets and execution view
```

## What's next

- [How onelf Thinks](./concepts): why a bundle carries its own libc and
  takes the GPU drivers from the host
- [AppDir Layout](./appdir-layout): how the directory is structured
- [Recipe File](./recipe): declarative packaging, imports and set
  selection
- [Execution Modes](./execution-modes): how the runtime actually runs things
- [Self-Update](./self-update): signed delta updates over HTTPS
