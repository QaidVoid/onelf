# Developing Packages

`onelf run` is a dev-loop helper: it runs an AppDir directly without
packing. Iterate fast by tweaking files and re-running.

## First-time setup

Scaffold a recipe:

```bash
cd myapp/
onelf init --binary /path/to/mybinary
```

This writes `myapp/onelf.toml` with `command = "bin/mybinary"` and an
`[input]` section naming the binary, so the scaffold builds as it
stands: the first `--bundle` run imports it to `bin/mybinary` for you.
Without `--binary` the recipe has `command` only, and you put the
binary in place yourself, or let `bundle-libs --from-binary` do it.

Then run it, assembling on the first pass:

```bash
onelf run --bundle
```

Subsequent runs just exec from the prepared view:

```bash
onelf run
```

## Dev loop

```bash
# Edit recipe or source, rebuild the binary, then:
onelf run

# If you changed the recipe's sysroot, sets, rules or exclusions,
# or added new dependencies:
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

Anything after `--` goes to the app. Give the PATH explicitly when you
pass args, so the first argument is not read as the path:

```bash
onelf run . -- --verbose --config ./dev.toml
```

Or for named entrypoints:

```bash
onelf run . --entrypoint myapp-daemon -- --port 8080
```

## Prepared development views

A development launch does not run out of your AppDir. It stages the
tree once, without compression, into a private view under the onelf
cache, `views/view-<key>/`, and execs the entrypoint from there. The
view is keyed by everything that changes what the launch shows or
restores: the tree's content digest, the selected entrypoint and its
sets, the host-library policy, the resolved links, and the environment
record the launch carries. Identical inputs reuse the same view
untouched; any change to any of them prepares another one, and the old
views are reclaimed by `onelf cache gc` and `onelf cache clear` once no
launch holds a lease. It is the same layout a packed launch's
execution view uses; see
[Execution views](./sysroot#execution-views).

The staged view carries the evaluated runtime mappings, the library
links the launch resolved, and the recipe's absolute-path overlays, so
the environment the entrypoint starts with is the one a packed launch
starts with.

The staging is cheap and safe at the same time. Files are hardlinked
into the view, and copied where your tree and the cache live on
different filesystems, so the view never shares a file the next edit
would rewrite. The view is built in a private staging tree and
published in one step: a launch either reuses a complete view or
prepares another one, never a half-built tree. Preparing the view never
writes into your AppDir; a `--bundle` run stages the recipe's assembly
into it, exactly as a build does.

While the app runs, the view stays claimed: the run takes the view's
lease and the entrypoint inherits it, so `onelf cache gc` and
`onelf cache clear` leave the view alone until the app and everything it
started are gone. See [The Lifetime Guardian](./guardian) for how such
claims behave across a daemonizing app.

`--bundle` assembles through the same machinery a build uses: the
recipe's sysroot, sets, rules, exclusions and entrypoints resolve once
into the request a build would pack from, and the selected entrypoint's
sets and the pinned GL build are acquired exactly as a launch would.
A recipe a build would refuse is refused here too, while the AppDir
stands untouched.

A run without `--bundle` assembles nothing: it runs the tree as it
stands and does not fall back to host libraries to conceal an
incomplete AppDir. A library neither the tree nor the selected sets
supply fails at load, by name.

Two messages name the dev-loop's own limits before anything runs:

```
onelf run: --bundle skipped: no recipe found in AppDir
```

`--bundle` needs a recipe to assemble from; without one the tree runs
as it stands. And when the entrypoint's file is not in the tree at all,
a recipe that stages its contents says so:

```
error: entrypoint target not found: .../bin/app (the recipe stages its contents: run with --bundle, or build first)
```

## What onelf run sets up

Same as a real packed run, minus the mount:

- `ONELF_DIR` (the view it prepared), `ONELF_ACTIVE_MODE=dev`,
  `ONELF_ARGV0`, `ONELF_EXEC`, `ONELF_ENTRYPOINT` and
  `ONELF_ENTRYPOINT_EXPORTED`. `ONELF_LAUNCH_DIR`
  is not set. When the recipe's
  `working-dir` moves the app away from your shell's directory, that
  directory is passed as `ONELF_USER_CWD` instead.
- The library search the packed runtime resolves, with one difference
  in transport: a packed launch places the resolver's farm and the
  sets' libraries inside the tree it serves and sets no
  `LD_LIBRARY_PATH` at all (see
  [Environment](./environment#library-paths)), while a development run
  does not write into your AppDir, so the farm, the view's library
  directories and the sets' live outside the tree and are exported on
  `LD_LIBRARY_PATH` when they are in play. A plain AppDir with no
  resolver findings and no sets gets none of it, like a packed run.
  The same per-library decision as a packed run decides what the host
  supplies (see
  [Bundling](./bundling#libraries-that-come-from-the-host)), with one
  difference: the AppDir is your own tree and there is no mount
  namespace, so a newer host copy of a library you also bundle cannot
  be placed over the bundled one. It is reported, and a packed run
  does it.
- `XDG_DATA_DIRS` prepended with `share/`
- `LIBGL_DRIVERS_PATH`, `LIBVA_DRIVERS_PATH`, `GBM_BACKENDS_PATH`
  if the corresponding subdirs exist

⚠️ Capability limits a development launch states outright, on the
terminal, every run:

- The recipe's `preload` list is opened by the packaged constructor,
  which a development launch does not carry.
- Only the initial environment is prepared; a re-exec that clears the
  environment is not hooked.
- The recipe's `[paths]` overlays are applied to the view when the
  launch can, and reported either way:

```
onelf run: the view's directories answer for the absolute paths the recipe built with
onelf run: the recipe's absolute paths are not overlaid: this launch could not
           make the view's directories answer for them, so the application reads the host's
```

A binary whose bootstrap `bundle-libs` injected maps its bundled loader
itself and is executed as is. A binary that still names a loader in
`PT_INTERP` is started through the bundled `ld-musl` or `ld-linux` when
the AppDir carries one, just like the packed runtime does.

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

`bundle-libs` also rewrites the binaries in the AppDir in place: their
`RUNPATH`, and the loader bootstrap it injects. For a truly clean start,
copy fresh binaries into `bin/` as well.
