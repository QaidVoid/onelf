# Recipe Schema

Full schema for `onelf.toml`. See [Recipe File](../guide/recipe) for the
user-facing guide.

## Top level

```toml
[package]
# required fields, plus optional metadata

[[entrypoint]]
# zero or more extra entrypoints

[compression]
# optional, sensible defaults

[update]
# optional, enables self-update

[bundle]
# optional, passes to bundle-libs

[env]
# optional, custom environment variables set before exec

# preload = [...]   optional, libs dlopen'd on every exec (top-level key)
```

Unknown fields are rejected (`deny_unknown_fields`) to catch typos.

## `[package]`

```toml
[package]
name = "myapp"                    # Option<String>, defaults to basename of command
command = "bin/myapp"             # String, required
output = "myapp.onelf"            # Option<PathBuf>, relative to recipe dir
working-dir = "inherit"           # "inherit" | "package" | "command"
memfd = true                      # Option<bool>, overrides auto-detect
exclude = ["*.a", "__pycache__"]  # Vec<String>, path globs
version = "1.2.3"                 # Option<String>
description = "..."               # Option<String>
license = "MIT"                   # Option<String>
homepage = "https://..."          # Option<String>
```

`working-dir` values:

- `inherit`: preserve the caller's cwd
- `package`: cwd is the AppDir root at runtime
- `command`: cwd is the directory containing the entrypoint binary

## `[[entrypoint]]`

```toml
[[entrypoint]]
name = "myapp-daemon"   # String, required; selected via argv[0]
path = "bin/myapp"      # String, required; relative to AppDir
args = ["--daemon"]     # Vec<String>, prepended to invocation
default = false         # bool, mark as default entrypoint
```

The `[package].command` entrypoint is always added first and implicitly
default unless another entry has `default = true`.

## `[compression]`

```toml
[compression]
level = 12            # i32, 0..=22, default 12
block-size = 262144   # u64 bytes, 4 KiB..=32 MiB, default 256 KiB
dict = false          # bool, default false
store = false         # bool, default false
```

Higher levels pack tighter but are slower. `dict = true` trains a shared
zstd dictionary across blocks, which helps ratios for packages with many
small text files.

`block-size` sets how much data each payload block holds. Blocks are
compressed independently, so the block size is the window zstd gets: a
package of large, sequentially-read files (shared libraries, game assets)
packs noticeably smaller at 1 MiB or 4 MiB. It is also the unit the runtime
decompresses to serve a read, so a package read at random pays for the
whole block to answer a few bytes. Leave it at the default unless the
package is big and read front-to-back.

`store = true` writes the payload raw with no zstd at all, trading
package size for zero decompression at runtime. It overrides `dict` and
`level`. Useful when the payload is dominated by already-compressed or
incompressible assets. The manifest stays compressed regardless (it is
small and separate from the payload).

## `[update]`

```toml
[update]
url = "https://releases.example.com/myapp.onelf.zsync"
key = "myapp.pub"      # optional path, required for self-update
embed = true           # optional, false for an external updater
```

When present, onelf selects the update-capable runtime, 1.36 MB larger
than slim. `key` is the 32-byte Ed25519 public key from `onelf key new`;
the runtime refuses to self-update without it. `embed = false` records
the URL and key but links the slim runtime, for packages updated by a
package manager.

## `[bundle]`

```toml
[bundle]
lib-dirs = ["auto"]              # default ["auto"]; listed dirs become LD_LIBRARY_PATH
search-paths = [
  "${MUSL_LIBDRM}/lib",
  "/opt/custom/lib",
]                                # Vec<String>, supports ${VAR}
exclude = ["libpthread"]         # Vec<String>, soname prefixes to skip
include = ["libfoo.so.1"]        # Vec<String>, force-include sonames
gl = false                       # auto-enabled from DT_NEEDED
dri = false                      # auto-enabled
vulkan = false                   # auto-enabled
wayland = false                  # auto-enabled
gtk = false                      # auto-enabled
no-gl = false                    # force-off, overrides auto-detect + gl
no-dri = false                   # force-off
no-vulkan = false                # force-off
no-wayland = false               # force-off
no-gtk = false                   # force-off
strip = false                    # run strip --strip-unneeded
strict-libc = false              # skip wrong-family libc libs
scan-dlopen = false              # scan strings for common dlopen'd libs
dlopen = ["libmyvendor.so.1"]    # extra sonames for scan-dlopen allow-list
skip = false                     # don't run bundle-libs at all (pre-bundled AppDir)
```

## `[env]`

```toml
[env]
PYTHONHOME = "${ONELF_DIR}/python"
RESVG_LIB_DIR = "${ONELF_DIR}/lib"
QT_PLUGIN_PATH = "${ONELF_DIR}/lib/qt6/plugins"
```

Each `KEY = "VALUE"` pair becomes an env var set before `exec`.
`${ONELF_DIR}` expands at runtime to the package root (FUSE mount,
tmpfs, or cache dir). Other `${VAR}` expand at **recipe-load** time
against the packer's environment; `$$` is an escape for a literal `$`,
so `$${VAR}` reaches the package as `${VAR}` and expands against the
**live** environment at runtime. Use this to prepend instead of
replace. POSIX `${VAR:-word}` is supported (use `word` if `VAR` is
unset or empty).

`PATH` defaults to `${ONELF_DIR}/bin:$${PATH:-/usr/bin:/bin}` (the
package's `bin/` prepended, re-exec-safe; falls back to `/usr/bin:/bin`
when the inherited PATH is empty, since onelf setting `PATH` suppresses
glibc's `_CS_PATH` fallback) unless `[env]` sets `PATH` explicitly, in
which case that value is used verbatim (`PATH = "$${PATH}"` opts out of
the prefix).

`.onelf/env` is also re-applied by the bundled `onelf-env` constructor
(`DT_NEEDED` on the entrypoint), so these survive a sandboxed
`clearenv()` + re-exec. The constructor is wired only when `patchelf`
is available at pack time and an `onelf-env` blob exists for the target
arch; otherwise the values are runtime-only (first launch).

## `preload`

```toml
preload = ["${ONELF_DIR}/lib/libpreload.so", "libfoo.so"]
```

Top-level array of libraries `dlopen`'d on every exec by the
`onelf-env` constructor. `${ONELF_DIR}` expands at runtime. Survives
sandboxed re-exec (baked into the ELF via `DT_NEEDED`), unlike
`LD_PRELOAD`. CLI: `--preload PATH` (repeatable).

## Env var expansion

All string fields support `${VAR}` expansion from the process's
environment at recipe load time. Missing variables stay literal so
`${ONELF_DIR}` and similar runtime placeholders pass through to the
runtime.
