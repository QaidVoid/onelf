# Quick Start

Turn a Linux binary into a single portable file in under a minute. This
page assumes `onelf` is [installed](./installation) and that the binary
runs on your machine.

## The three-step version

1. **Scaffold** an AppDir from the binary:

   ```bash
   onelf bundle-libs ./mypkg --from-binary /path/to/mybinary --scan-dlopen
   ```

   This copies the binary to `./mypkg/bin/mybinary`, reads the libraries
   it links (`DT_NEEDED`) and the commonly `dlopen`ed libraries named in
   its strings, and copies every one it needs into `./mypkg/lib/`. Mesa
   and the other GPU drivers are left out on purpose: the packed app uses
   the host's.

2. **Pack** the AppDir into one executable:

   ```bash
   onelf pack ./mypkg -o mybinary.onelf --command bin/mybinary
   ```

   `--command` is the path of the binary inside the AppDir.

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

## If something is missing

A library the app loads by a computed name at run time is invisible to
the scan in step 1, and the packed app fails on another machine with
`cannot open shared object file`. Let the app show what it loads:

```bash
onelf bundle-libs ./mypkg --target bin/mybinary --trace-run
```

This runs the binary for a few seconds and bundles every library it
opened. Then pack again. [Bundling Libraries](./bundling) covers the
other ways to find and add libraries.

## The recipe version

For anything reused or committed to a repo, write the settings down in
an `onelf.toml` inside the AppDir:

```bash
cd mypkg
onelf init --binary bin/mybinary
# Edit onelf.toml to taste, then:
onelf build
```

`onelf init` writes a commented starter recipe whose `command` is
`bin/mybinary`. It does not copy the binary, so run it in an AppDir that
already holds one. `onelf build` runs `bundle-libs` and `pack` with the
recipe's settings and writes `mybinary.onelf` next to the recipe. See
the [Recipe File](./recipe) guide.

## Dev loop

Run the AppDir in place, without packing:

```bash
onelf run --bundle       # bundle libs, then run
onelf run                # subsequent runs: just exec
```

## Inspect

```bash
onelf info mybinary.onelf     # metadata, entrypoints, sizes
onelf list mybinary.onelf     # file tree
onelf verify mybinary.onelf   # BLAKE3 integrity check
```

## What's next

- [How onelf Thinks](./concepts): why a bundle carries its own libc and
  takes the GPU drivers from the host
- [AppDir Layout](./appdir-layout): how the directory is structured
- [Recipe File](./recipe): declarative packaging
- [Execution Modes](./execution-modes): how the runtime actually runs things
- [Self-Update](./self-update): signed delta updates over HTTPS
