# Recipe File

`onelf.toml` is a declarative recipe that replaces long CLI invocations.
One file captures everything needed to reproduce a pack.

## Minimal recipe

```toml
[package]
name = "myapp"
command = "bin/myapp"
```

Drop this next to the AppDir and run:

```bash
onelf build
```

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
level = 12
dict = false

[update]
url = "https://releases.example.com/amdgpu_top.onelf.zsync"

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
| `memfd` | bool | auto | Force memfd-eligible (auto-detected otherwise) |
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

### `[compression]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `level` | int | `12` | Zstd level, 0 to 22 |
| `dict` | bool | `false` | Train a shared dictionary for better ratio |
| `store` | bool | `false` | Store payload raw, no zstd (overrides `dict`/`level`) |

### `[update]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | string | none | zsync control file URL |
| `key` | path | none | 32-byte Ed25519 public key; required for self-update |
| `embed` | bool | `true` | Embed the updater. `false` records the URL for an external updater |

Setting `[update]` selects the update-capable runtime, which is 1.36 MB
larger than the slim runtime. Self-update also needs `key`: without it
the runtime refuses to update. Set `embed = false` to keep the metadata
and the slim runtime, for packages a package manager updates. See
[Self-Update](./self-update).

### `[bundle]`

Everything `bundle-libs` accepts, as recipe keys.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `lib-dirs` | array | `["auto"]` | Bundled directories the runtime adds to its lib search (pass `["auto"]` to detect) |
| `search-paths` | array | `[]` | Extra lib search paths (highest priority) |
| `exclude` | array | `[]` | Excluded soname prefixes |
| `include` | array | `[]` | Force-included sonames |
| `gl` / `dri` / `vulkan` / `wayland` / `gtk` | bool | auto | Framework bundlers |
| `strip` | bool | `false` | Run `strip` on bundled libs |
| `strict-libc` | bool | `false` | Refuse wrong-family libc libs |
| `scan-dlopen` | bool | `false` | Scan for dlopen'd libs in binary strings |
| `dlopen` | array | `[]` | Extra sonames for `scan-dlopen` allow-list |
| `skip` | bool | `false` | Don't run bundle-libs at all |

### `[env]`

Custom environment variables set by the runtime before exec.

```toml
[env]
PYTHONHOME = "${ONELF_DIR}/python"
QT_PLUGIN_PATH = "${ONELF_DIR}/lib/qt6/plugins"
GST_PLUGIN_PATH_1_0 = "${ONELF_DIR}/lib/gstreamer-1.0"
```

`${ONELF_DIR}` expands to the package root at runtime (the FUSE
mount, tmpfs, or cache dir depending on execution mode), so values
follow the running app wherever it lives. Other `${VAR}` references
expand at recipe-load time against the **packer's** environment.

To defer a reference to **runtime** instead, escape the `$` with `$$`:
`$${VAR}` reaches the package as `${VAR}` and is expanded against the
**live** process environment when the app runs. POSIX
`${VAR:-word}` is supported: if `VAR` is unset *or empty*, the literal
`word` is used instead. This is how you *prepend* rather than replace:

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

so bundled helpers resolve even when the app or a sandbox clears
`PATH`. (glibc's `_CS_PATH` fallback of `/bin:/usr/bin` only applies
when `PATH` is *unset*; the moment onelf sets `PATH` it no longer
kicks in, so the `:-/usr/bin:/bin` default substitutes it explicitly
when the inherited `PATH` is empty, instead of leaving a dangling
empty element.) Set `[env] PATH` yourself to take full control (the
default is then skipped); use `PATH = "$${PATH}"` to opt out of the
`bin/` prefix entirely.

These variables are **re-exec-safe**: a small `onelf-env` constructor
is bundled into `lib/` and injected as a `DT_NEEDED` of the entrypoint,
so `.onelf/env` is re-applied on every exec, including after an app
re-execs itself in a sandbox that calls `clearenv()` (Chromium/Electron
zygotes, Steam, bwrap, etc.). This requires `patchelf` at pack time;
without it the values are only set for the first launch. See
[Environment › Surviving a sandboxed re-exec](./environment#surviving-a-sandboxed-re-exec).

### `preload`

A top-level list of libraries to `dlopen` on every exec, via the same
`onelf-env` constructor. Unlike `LD_PRELOAD` (an env var, wiped by a
sandboxed re-exec), this is baked into the ELF and survives.

```toml
preload = ["${ONELF_DIR}/lib/libfoo.so", "libbar.so"]
```

`${ONELF_DIR}` expands at runtime. CLI equivalent: `--preload PATH`
(repeatable).

## Environment variable expansion

Any string in `[bundle] search-paths` (and other path fields) expands
`${VAR}` references at parse time:

```toml
[bundle]
search-paths = ["${MUSL_LIBDRM}/lib", "${HOME}/my-libs"]
```

Missing variables expand to empty strings.

## Path resolution

Relative paths in the recipe resolve against the recipe's directory,
not the caller's cwd. This means running `onelf build` from anywhere
always produces the same result.

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

For anything more specific, edit the recipe directly.
