# Developing Packages

`onelf run` is a dev-loop helper: it runs an AppDir directly without
packing. Iterate fast by tweaking files and re-running.

## First-time setup

Scaffold a recipe:

```bash
cd myapp/
onelf init --binary /path/to/mybinary
```

This writes `myapp/onelf.toml` seeded with sensible defaults.

Then run it, bundling libs on the first pass:

```bash
onelf run --bundle
```

Subsequent runs just exec (libs are already bundled):

```bash
onelf run
```

## Dev loop

```bash
# Edit recipe or source, rebuild the binary, then:
onelf run

# If you added new dependencies:
onelf run --bundle
```

## Running without a recipe

If your AppDir has no `onelf.toml` but has a single binary under `bin/`,
`onelf run` auto-detects it:

```bash
onelf run ./myapp
```

If there are multiple binaries, pass `--command`:

```bash
onelf run ./myapp --command bin/helper
```

## Passing arguments

Anything after `--` goes to the app:

```bash
onelf run -- --verbose --config ./dev.toml
```

Or for named entrypoints:

```bash
onelf run --entrypoint myapp-daemon -- --port 8080
```

## What onelf run sets up

Same as a real packed run, minus the mount:

- `ONELF_DIR`, `ONELF_ACTIVE_MODE=dev`, `ONELF_ARGV0`, etc.
- `LD_LIBRARY_PATH` prepended with the resolver's link farm and the
  AppDir's lib directories. The same per-library decision as a packed
  run decides what the host supplies (see
  [Bundling](./bundling#libraries-that-come-from-the-host)), with one
  difference: the AppDir is your own tree and there is no mount
  namespace, so a newer host copy of a library you also bundle cannot be
  placed over the bundled one. It is reported, and a packed run does it.
- `XDG_DATA_DIRS` prepended with `share/`
- `LIBGL_DRIVERS_PATH`, `LIBVA_DRIVERS_PATH`, `GBM_BACKENDS_PATH`
  if the corresponding subdirs exist

For cross-libc packages, `onelf run` detects a missing `PT_INTERP` and
invokes the bundled `ld-musl` or `ld-linux` directly, just like the
packed runtime does.

## Packing for distribution

Once the recipe is stable:

```bash
onelf build                  # produces myapp.onelf
onelf verify myapp.onelf     # sanity check
```

## Clean / reset

The `lib/` directory and `.onelf/` metadata are managed by onelf. To
start fresh, delete them:

```bash
rm -rf myapp/lib myapp/.onelf myapp/myapp.onelf
onelf build
```
