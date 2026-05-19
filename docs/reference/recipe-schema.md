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
level = 12     # i32, 0..=22, default 12
dict = false   # bool, default false
store = false  # bool, default false
```

Higher levels pack tighter but are slower. `dict = true` trains a shared
zstd dictionary across blocks, which helps ratios for packages with many
small text files.

`store = true` writes the payload raw with no zstd at all, trading
package size for zero decompression at runtime. It overrides `dict` and
`level`. Useful when the payload is dominated by already-compressed or
incompressible assets. The manifest stays compressed regardless (it is
small and separate from the payload).

## `[update]`

```toml
[update]
url = "https://releases.example.com/myapp.onelf.zsync"
```

When present, onelf selects the update-capable runtime (~1.3 MB larger
than slim).

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

Each `KEY = "VALUE"` pair becomes an env var set by the runtime
before `exec`. Values support `${ONELF_DIR}` expansion at runtime,
which resolves to the package root (FUSE mount, tmpfs path, or cache
dir depending on execution mode). Other `${VAR}` references are
expanded against the user's environment at recipe-load time.

## Env var expansion

All string fields support `${VAR}` expansion from the process's
environment at recipe load time. Missing variables stay literal so
`${ONELF_DIR}` and similar runtime placeholders pass through to the
runtime.
