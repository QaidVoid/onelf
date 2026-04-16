# Quick Start

Turn any Linux binary into a single portable file in under a minute.

## The three-step version

1. **Scaffold** an AppDir from a single binary:

   ```bash
   onelf bundle-libs ./mypkg --from-binary /path/to/mybinary --scan-dlopen
   ```

   This creates `./mypkg/bin/mybinary`, scans its `DT_NEEDED` and common
   `dlopen` targets, and copies every shared library it needs into `./mypkg/lib/`.

2. **Pack** the AppDir into one executable:

   ```bash
   onelf pack ./mypkg -o mybinary.onelf --command bin/mybinary
   ```

3. **Run** it:

   ```bash
   ./mybinary.onelf --any --args --you --like
   ```

The runtime mounts the package as a private FUSE filesystem in a user
namespace, `exec`s the target, and lets the kernel clean everything up on exit.

## The recipe version

For anything reused or committed to a repo, use a recipe instead:

```bash
onelf init --binary /path/to/mybinary
# Edit onelf.toml to taste, then:
onelf build
```

`onelf.toml` captures bundle settings, entrypoints, compression, metadata,
update URL, and so on. See the [Recipe File](./recipe) guide.

## Dev loop

Iterate on a recipe without packing:

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

- [AppDir Layout](./appdir-layout) — how the directory is structured
- [Recipe File](./recipe) — declarative packaging
- [Execution Modes](./execution-modes) — how the runtime actually runs things
- [Self-Update](./self-update) — delta updates over HTTP
