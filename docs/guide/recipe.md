# Recipe File

`onelf.toml` is a declarative recipe that replaces long CLI invocations.
One file captures everything needed to reproduce a pack.

The rules this page encodes:

- ✅ Write the pack as a recipe and run `onelf build`. One file
  reproduces it, with no long CLI invocation to retype.
- ✅ `[input] binary` to start from a bare binary. The build copies it
  to the command's path. The source is never modified, whether it
  lives outside the recipe's directory or inside it: the bundling walk
  rewrites only the copy at the command's path.
- ✅ `${ONELF_DIR}` for values that must follow the package. Any other
  `${VAR}` the packer's environment sets is expanded at pack time.
- ✅ `$ONELF_DIR/bin` is prepended to `PATH` by default. Set
  `[env] PATH` to take full control.
- ⚠️ `[update]` switches the package to the considerably larger
  update-capable runtime; set `embed = false` when a package manager
  does the updating.
- ⚠️ Env values survive a sandboxed re-exec only when `patchelf` was
  available at pack time. Without it they apply to the first launch
  only.

## Minimal recipe

```toml
[package]
command = "bin/myapp"
```

`command` is the only required field in the file. Drop this next to an
AppDir that already holds the binary and run:

```bash
onelf build
```

## Starting from a bare binary

An AppDir you have not assembled yet can start from the upstream
binary alone. `[input]` imports it to the command's path before
anything else runs:

```toml
[package]
command = "bin/myapp"

[input]
binary = "../upstream/myapp"
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `binary` | path | none | A bare executable copied to the package command's path before assembly, relative to the recipe's directory |

- ✅ The import never writes the source, and neither does the rest of
  the build: it reads, hashes and copies to the command's path, and a
  symlink in the destination's way is refused rather than followed.
  The bundling walk keeps an in-tree source out of its rewriting too,
  so the file you gave stays byte for byte what it was.
- ✅ The staged copy is tracked by content hash: a rebuild refreshes it
  from its source, and a copy someone edited in the AppDir stops the
  build instead of being overwritten or packed stale.
- ❌ `[input] binary` and `[sysroot] path` together. A build takes its
  contents from one authoritative source, so naming both fails before
  either is read.
- ❌ `[input] binary` with `[bundle] skip`. Skip expects an already
  prepared tree, so there is nothing to import into.
- ✅ `[input] binary` with a `[sysroot]` that only declares sets (no
  `path`). The tree has no package database to consult, so the import
  and your own additions are the whole bundle.
- ❌ An archive, a package-manager identifier or a source tree. A bare
  executable file is the only supported input, and anything else is
  refused with what the supported forms are.
- ❌ A symlink in the way of the destination, or a command path that
  escapes the AppDir. The build fails before copying or modifying any
  file outside it.
- ❌ A source that is the command itself: the same path, a symlink to
  it, or a hardlink of it. Importing would rewrite the source, so the
  recipe is refused before anything is written. Drop `[input]` when the
  AppDir already carries the command.

## Full example

```toml
[package]
name = "amdgpu_top"
command = "bin/amdgpu_top"
output = "amdgpu_top.onelf"
version = "0.11.3"
description = "Tool to display AMDGPU usage"
license = "MIT"
homepage = "https://github.com/Umio-Yasuno/amdgpu_top"
working-dir = "package"

[[entrypoint]]
name = "amdgpu_top-smi"
path = "bin/amdgpu_top"
args = ["--smi"]

[[entrypoint]]
name = "amdgpu_top-gui"
path = "bin/amdgpu_top"
args = ["--gui"]

[compression]
level = 19
dict = false

[update]
url = "https://releases.example.com/amdgpu_top.onelf"

[bundle]
search-paths = ["${MUSL_LIBDRM}", "${MUSL_GCC}"]
strict-libc = true
scan-dlopen = true
dlopen = ["libmyvendor.so.1"]
```

## Sections

### `[package]`

The main entry. Covers the default entrypoint's command, output file,
runtime behavior, and metadata.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | derived from `command` | Package name |
| `command` | string | required | Path to main binary, relative to AppDir |
| `output` | path | `{name}.onelf` | Where to write the packed file |
| `working-dir` | enum | `inherit` | `inherit`, `package`, or `command` |
| `host-libs` | string | `auto` | `auto`, `always`, `never`: what the launch resolver may take from the host. See [Bundling](./bundling#libraries-that-come-from-the-host) |
| `cache` | bool | `false` | Allow the persistent cache as the last execution mode. See [Execution Modes](./execution-modes#mode-6-persistent-cache-only-on-request) |
| `prefer-cache` | bool | `false` | Run from the persistent cache before any other execution mode. Implies `cache`. See [Execution Modes](./execution-modes#an-app-that-records-its-own-path) |
| `memfd` | bool | auto | Force memfd-eligible (auto-detected otherwise) |
| `needs-setuid` | bool | `false` | The app runs setuid binaries such as `sudo` or `pkexec`, so keep it out of a user namespace |
| `sets` | array | all declared | Which declared shared sets the package command uses. See [Selecting dependency sets](#selecting-dependency-sets) |
| `mtime` | int | none | Pin every entry's mtime to this Unix timestamp. See [Reproducible Builds](./reproducible) |
| `exclude` | array | `[]` | Glob patterns to exclude from the pack |
| `version` | string | none | Shown by `onelf info` |
| `description` | string | none | Shown by `onelf info` |
| `license` | string | none | Shown by `onelf info` |
| `homepage` | string | none | Shown by `onelf info` |

### `[[entrypoint]]`

Additional named entrypoints. Each one becomes accessible via argv[0]
matching: symlink the packed binary to that name and it runs the matching
entrypoint automatically.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | required | Entrypoint name (argv[0] match) |
| `path` | string | required | Path to binary within AppDir |
| `args` | array | `[]` | Args always prepended to the invocation |
| `default` | bool | `false` | Run this one by default |
| `sets` | array | all declared | Which declared shared sets this entrypoint uses. See [Selecting dependency sets](#selecting-dependency-sets) |

### Selecting dependency sets

A recipe that declares shared sets under `[sysroot.sets.<name>]` can
name, per entrypoint, which of them the entrypoint uses. `sets` is a
list of declared set names in `[package]` for the package command and
in each `[[entrypoint]]` for that entrypoint. The defaults are exact:

- ✅ A list that is **omitted selects every declared set**. This is
  what every recipe without the key has always done, and the package
  stays byte-identical to what the same recipe always packed.
- ✅ **`sets = []` selects none.** The launch fetches, mounts and
  extracts no set at all, and does not fail because an unselected set
  is unavailable. Use it for a CLI entrypoint beside a GUI one.
- ✅ **A written list selects exactly the names it lists**, whatever
  order they are written in: the launch reads them in canonical (sorted)
  declaration order.
- ✅ A named entrypoint's omitted list does **not** inherit the package
  command's explicit list. Every entrypoint's list is its own answer.
- ❌ A `[package] sets` list beside an entrypoint marked `default = true`.
  That entrypoint takes the package command's place in the package, so
  the list would select for nothing. Write it as the default
  entrypoint's own `sets`.

```toml
[package]
command = "bin/app"
sets = ["gtk"]                  # the GUI keeps its toolkit

[[entrypoint]]
name = "app-cli"
path = "bin/app"
args = ["--cli"]
sets = []                       # the CLI fetches no set
```

❌ A name the recipe does not declare, or one written twice in the same
list, fails when the recipe loads, naming the list it was written in.
✅ The package records the exact selections, and `onelf info` prints
them under `Sets:`; `--onelf-doctor` reports, per entrypoint, which
sets a launch would fetch and whether the store already holds each.
⚠️ A recipe that writes any explicit list, empty or not, needs a
runtime that advertises payload metadata and selection-view support;
packing refuses a substituted runtime that could not enforce the
selections. A recipe with every list omitted stays packable anywhere.

At launch, only the sets the invoked entrypoint selects are obtained. A
required selected set that cannot be acquired ends the launch naming
the set and the reason; the optional pinned GL build keeps its
separate, warning-only behavior. At build time the declared sets are
audited against each other, so two sets no entrypoint could select
together stop the build before anything is written (see
[Sharing a dependency set](./sysroot#sharing-a-dependency-set)).

### `[compression]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `level` | int | `19` | Zstd level, 0 to 22 |
| `block-size` | int | `65536` | Bytes per payload block, 4 KiB to 32 MiB. A stipplefs payload takes a power of two up to 64 MiB and defaults to 32 MiB |
| `payload` | string | `blocks` | `blocks` or `stipple`. See [the file format](../reference/file-format#stipplefs-payload) |
| `dict` | bool | `false` | Train a shared dictionary for better ratio |
| `store` | bool | `false` | Store payload raw, no zstd (overrides `dict`/`level`) |
| `prefetch` | path | none | Startup profile recorded with `ONELF_PROFILE`, relative to the recipe. See [Prefetching a startup](./execution-modes#prefetching-a-startup) |

### `[update]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | string | required | `https://` URL of the release binary itself, where updates are fetched from |
| `key` | path | none | 32-byte Ed25519 public key, or several concatenated as a keyring. Required for signed self-update. A secret key is refused |
| `revision` | int | none | Release number. The runtime refuses an update that is not strictly newer |
| `unsigned` | bool | `false` | Accept updates without signature verification, warning on stderr. Trusts the release server and cannot be combined with `key` |
| `embed` | bool | `true` | Embed the updater. `false` records the URL for an external updater |

The problem: `[update]` selects the update-capable runtime, which is
considerably larger than the slim runtime (exact sizes in the
[Self-Update size table](./self-update#size-tradeoff)), and self-update
also needs `key` or an explicit `unsigned = true` opt-in.

- ❌ Self-update without `key` or `unsigned = true`. The runtime refuses to update.
- ✅ `embed = false` when a package manager updates the package: the
  URL is recorded for an external updater and the slim runtime is kept.
- ✅ The default `embed = true` when the package updates itself.

See [Self-Update](./self-update).

### `[bundle]`

Everything `bundle-libs` accepts, as recipe keys.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `lib-dirs` | array | `["auto"]` | Bundled directories the runtime adds to its lib search (pass `["auto"]` to detect) |
| `search-paths` | array | `[]` | Extra lib search paths (highest priority), relative to the recipe's directory |
| `exclude` | array | `[]` | Excluded soname prefixes |
| `include` | array | `[]` | Force-included sonames |
| `gl` / `dri` / `vulkan` | bool | `false` | Bundle the GPU driver stack. Off by default, so the host's is used. An error with `[sysroot]` |
| `wayland` / `gtk` | bool | auto | Client-side framework bundlers, enabled on detection. An error with `[sysroot]` |
| `no-gl` / `no-dri` / `no-vulkan` / `no-wayland` / `no-gtk` | bool | `false` | Force a framework off, overriding detection and the matching key |
| `strip` | bool | `false` | Run `strip` on bundled libs |
| `strict-libc` | bool | `false` | Refuse wrong-family libc libs |
| `scan-dlopen` | bool | `false` | Scan for dlopen'd libs in binary strings |
| `dlopen` | array | `[]` | Extra sonames for `scan-dlopen` allow-list |
| `trace-run` | bool | `false` | Run the command and bundle every library it loaded. See [Bundling](./bundling#detecting-dlopen-d-libs) |
| `trace-seconds` | int | `5` | How long the traced run may go |
| `trace-args` | array | `[]` | Arguments for the traced run |
| `skip` | bool | `false` | Don't run bundle-libs at all: the tree stands as authored, unchecked. See [Unchecked trees](#unchecked-trees) |

### `[sysroot]`

Take the bundle's contents from a pinned sysroot instead of scanning the
packer's machine. Paths are relative to the recipe. See
[Bundling from a Sysroot](./sysroot).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | path | none | The materialized rootfs, outside the AppDir. Omit it to record sets only, with no package database |
| `archive` | path | none | `.tar` or `.tar.zst` materialized into `path` when it does not exist, a path or an `https://` URL |
| `hash` | string | none | BLAKE3 the archive must have before it is unpacked |
| `platform` | string | archive or directory name | Label recorded as the package's platform in its provenance |
| `optional` | array | `[]` | Optional dependencies to include, by package name |
| `platform-line` | path | none | File of soname prefixes the host provides |
| `policy` | path | none | File of glob patterns that never ship |
| `trace` | path | none | File of paths a test run opened |
| `keep` | path | none | File of glob patterns the trace may not prune |
| `platform-url` / `platform-hash` | string | none | Override the GL build the sysroot pins |

Shared dependency sets go in `[sysroot.sets.<name>]` tables. See the
[Recipe Schema](../reference/recipe-schema#sysroot-sets-name) and
[Selecting dependency sets](#selecting-dependency-sets).

A `[sysroot]` that names a `path` needs no other input, and every
database key above belongs to that path: naming one without `path`
stops the recipe from loading, as does a `[sysroot]` with neither
`path` nor sets nor a GL build pin. When a path is present,
`bundle-libs` runs even if `[bundle] skip` is set, since it is what
applies the sysroot. The build also pins every mtime to 0 unless
`[package] mtime` or `SOURCE_DATE_EPOCH` is set.

### Unchecked trees

`[bundle] skip = true` packs the AppDir exactly as it stands: no scan,
no dependency audit, no completeness claim. It is the escape hatch for
a tree that was assembled and verified some other way, such as a
set-only recipe whose tree is an upstream release unpacked into place.

- ❌ Expecting skip to mean checked. Nothing about the tree is
  verified: the build validates the recipe's declared pins and the
  runtime's capabilities, and nothing more. What the tree lacks shows
  up at launch, as a missing library named at load time.
- ✅ A sysroot with a `path` is the checked route: a build from a
  package database stops on an unresolved `DT_NEEDED`, because the
  universe was complete and the omission is a policy mistake. See
  [Bundling from a Sysroot](./sysroot).
- ✅ A host scan sits between the two: unresolved dependencies are
  warnings there, since the scan's universe was a guess. The same is
  true of `onelf build` on an AppDir without a sysroot and without
  skip.

### `[paths]`

Maps an absolute path the application compiled into itself onto a
directory in the bundle. Prefer `[env]` when the application reads a
variable for that path. See
[Environment](./environment#making-the-bundle-answer-for-a-compiled-in-path).

```toml
[paths]
"/usr/share/galculator" = "share/galculator"
```

### `[env]`

Custom environment variables set by the runtime before exec.

```toml
[env]
PYTHONHOME = "${ONELF_DIR}/python"
QT_PLUGIN_PATH = "${ONELF_DIR}/lib/qt6/plugins"
GST_PLUGIN_PATH_1_0 = "${ONELF_DIR}/lib/gstreamer-1.0"
```

The problem: the package root exists only once the app runs, and a
plain `${VAR}` is expanded while the recipe loads, against the
**packer's** environment.

- ❌ `LD_LIBRARY_PATH` and the other loader search-path variables
  (`LD_LIBRARY_PATH_32`, `LD_LIBRARY_PATH_64`, `LD_AOUT_LIBRARY_PATH`).
  The recipe is refused when it assigns one: the loader search path is
  what onelf resolves, not a configurable one. Bundled binaries reach
  their libraries through the `$ORIGIN` run path the bundler writes,
  host and set libraries are linked by name into that same directory,
  and nothing the app spawns sees a generated search path. Point the
  build at a library with `[bundle] search-paths`, or let the
  resolver's host policy supply it, instead of wiring a path by hand.

  The refusal reads the same for all four names. A build prints:

```
error: ./onelf.toml: [env] LD_LIBRARY_PATH: refused: the loader search path is what onelf resolves, not a configurable one
```

  and `onelf run` prints, before it stages or launches anything:

```
error: [env] LD_LIBRARY_PATH cannot be set: the loader search path is what onelf resolves, not a configurable one; bundle the library with the AppDir or let the resolver choose it
```

- ✅ `${ONELF_DIR}` for anything under the package root. It expands at
  runtime to the FUSE mount, tmpfs, or cache dir the app runs from
  (depending on execution mode), so values follow the running app
  wherever it lives.
- ⚠️ Any other `${VAR}` expands at recipe-load time against the
  **packer's** environment.
- ✅ `$${VAR}` (escape the `$`) to defer a reference to **runtime**.
  It reaches the package as `${VAR}` and is expanded against the
  **live** process environment when the app runs.
- ✅ POSIX `$${VAR:-word}` for a runtime default: if `VAR` is unset
  *or empty*, the literal `word` is used instead.

```toml
[env]
PATH = "${ONELF_DIR}/bin:$${PATH}"   # bin/ first, keep the caller's PATH
```

**`$ONELF_DIR/bin` is added to `PATH` by default.** Every package
behaves as if it had:

```toml
[env]
PATH = "${ONELF_DIR}/bin:$${PATH:-/usr/bin:/bin}"
```

✅ That default is why bundled helpers resolve even when the app or a
sandbox clears `PATH`. glibc's `_CS_PATH` fallback of `/bin:/usr/bin`
only applies when `PATH` is *unset*; the moment onelf sets `PATH` it no
longer kicks in, so the `:-/usr/bin:/bin` default substitutes it
explicitly when the inherited `PATH` is empty, instead of leaving a
dangling empty element.

- ✅ Set `[env] PATH` yourself to take full control; the default is
  then skipped.
- ✅ `PATH = "$${PATH}"` to opt out of the `bin/` prefix entirely.

These variables are **re-exec-safe**: a small `onelf-env` constructor
is bundled into `lib/` and injected as a `DT_NEEDED` of the entrypoint,
so `.onelf/env` is re-applied on every exec, including after an app
re-execs itself in a sandbox that calls `clearenv()` (Chromium/Electron
zygotes, Steam, bwrap, etc.). ⚠️ This requires `patchelf` at pack time;
without it the values are only set for the first launch. See
[Surviving a sandboxed re-exec](./environment#surviving-a-sandboxed-re-exec).

### `preload`

A top-level list of libraries to `dlopen` on every exec, via the same
`onelf-env` constructor.

- ✅ `preload` over `LD_PRELOAD`. `LD_PRELOAD` is an env var, wiped by
  a sandboxed re-exec; this is baked into the ELF and survives.

```toml
preload = ["${ONELF_DIR}/lib/libfoo.so", "libbar.so"]
```

`${ONELF_DIR}` expands at runtime. CLI equivalent: `--preload PATH`
(repeatable).

## Environment variable expansion

Every string value in the recipe expands `${VAR}` references from the
packer's environment when the recipe loads:

```toml
[bundle]
search-paths = ["${MUSL_LIBDRM}/lib", "${HOME}/my-libs"]
```

⚠️ A variable the packer's environment does not set is kept as written,
not replaced with an empty string. That is how `${ONELF_DIR}` reaches
the runtime, but it also means a typo survives into the package.

## Path resolution

✅ Run `onelf build` from anywhere; the result is the same. Relative
paths in the recipe resolve against the recipe's directory, not the
caller's cwd.

```bash
onelf build             # looks for ./onelf.toml
onelf build path/to/app # looks for path/to/app/onelf.toml
onelf build app/recipe.toml  # any .toml file works
```

## Overriding at the CLI

A few flags override recipe values:

```bash
onelf build -o custom-output.onelf    # overrides [package] output
```

✅ For anything more specific, edit the recipe directly.
