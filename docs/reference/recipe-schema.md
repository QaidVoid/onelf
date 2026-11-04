# Recipe Schema

Full schema for `onelf.toml`. See [Recipe File](../guide/recipe) for the
user-facing guide.

## Top level

```toml
[package]
# required fields, plus optional metadata

[input]
# optional, imports a bare binary to the command's path

[[entrypoint]]
# zero or more extra entrypoints

[compression]
# optional, sensible defaults

[update]
# optional, enables self-update

[sysroot]
# optional, take the bundle from a package database, or record sets only

[bundle]
# optional, passes to bundle-libs

[paths]
# optional, maps an absolute path the app opens onto a bundled directory

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
host-libs = "auto"                # "auto" | "always" | "never": what the resolver may take from the host
cache = false                     # bool, allow the persistent cache as the last execution mode
prefer-cache = false              # bool, run from the persistent cache before any other mode; implies cache
needs-setuid = false              # bool, the app runs setuid binaries (sudo, pkexec); keeps it out of user namespaces
sets = ["gtk"]                    # Option<Vec<String>>, declared sets the command selects; see below
mtime = 1735689600                # Option<u64>, pin every entry's mtime to this Unix timestamp (overrides SOURCE_DATE_EPOCH)
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

## `[input]`

```toml
[input]
binary = "../upstream/myapp"      # PathBuf, relative to the recipe's directory
```

Imports a bare executable to the package command's path before
assembly, so a build can start from the upstream binary alone. The
import reads the source and never writes it, and the bundling walk
keeps an in-tree source out of its rewriting. A source that is the
command itself, by path, symlink or hardlink, is refused. The
staged copy is tracked by content hash, a rebuild refreshes it, and an
edited copy stops the build instead of being overwritten. `binary`
must be relative to the recipe's directory, must name a regular file,
and may not travel through a symlink or escape the AppDir. It
conflicts with `[sysroot] path` (one authoritative source) and with
`[bundle] skip` (nothing to import into), and is accepted beside a
`[sysroot]` that only declares sets. Archives, package-manager
identifiers and source trees are refused. See [Recipe
File](../guide/recipe#starting-from-a-bare-binary).

## `[[entrypoint]]`

```toml
[[entrypoint]]
name = "myapp-daemon"   # String, required; selected via argv[0]
path = "bin/myapp"      # String, required; relative to AppDir
args = ["--daemon"]     # Vec<String>, prepended to invocation
default = false         # bool, mark as default entrypoint
sets = ["gtk"]          # Option<Vec<String>>, declared sets this entrypoint selects
```

`[package].command` does not add an entrypoint of its own. It supplies
the package's default entrypoint: when no declared entrypoint has
`default = true`, entrypoint 0 is the command under its basename, with
the command's path and no args. When a declared entrypoint is marked
`default = true`, that entrypoint takes the default slot instead, under
its own name, path, and args, and the command contributes only the
package name fallback (`[package].name` defaults to the command's
basename).

## `[compression]`

```toml
[compression]
level = 19            # i32, 0..=22, default 19
block-size = 65536    # u64 bytes, 4 KiB..=32 MiB, default 64 KiB (stipplefs: power of two up to 64 MiB, default 32 MiB)
payload = "blocks"    # "blocks" | "stipple", default "blocks"
dict = false          # bool, default false
store = false         # bool, default false
prefetch = "profile.txt"   # path, relative to the recipe, optional
```

Higher levels pack tighter but are slower. `dict = true` trains a shared
zstd dictionary across blocks, which helps ratios for packages with many
small text files.

`block-size` sets how much data each payload block holds. Blocks are
compressed independently, so the block size is the window zstd gets, and
larger blocks pack smaller. It is also the unit the runtime decompresses
to serve a read, so a read pays for its whole block. Starting an app reads
scattered pages of its shared libraries: on Blender, 64 KiB started 17
percent faster from a cold cache than 256 KiB for a package 5 percent
larger, and 1 MiB was slower than 256 KiB by a further 40 percent. Leave
it at the default unless the package's large files are read front to
back, as game assets and media are.

`payload = "stipple"` stores the tree as a stipplefs image instead,
described in [the file format](./file-format#stipplefs-payload). Its
blocks hold many files each, deduplicated by content and compressed
together, so `block-size` there defaults to 32 MiB and may go to 64 MiB.
The runtime still verifies and caches 64 KiB slices of each file, and a
`prefetch` profile also orders the image, so what a startup reads sits
together at its front. `dict` does not apply: the image trains its own.

`store = true` writes the payload raw with no zstd at all, trading
package size for zero decompression at runtime. It overrides `dict` and
`level`. Useful when the payload is dominated by already-compressed or
incompressible assets. The manifest stays compressed regardless (it is
small and separate from the payload).

`prefetch` names a profile of the blocks a startup reads, recorded with
`ONELF_PROFILE`. The packer writes the blocks it names to
`.onelf/prefetch`, and the runtime decompresses them ahead of the app. See
[Prefetching a startup](../guide/execution-modes#prefetching-a-startup).

## `[update]`

```toml
[update]
url = "https://releases.example.com/myapp.onelf"   # must be https://, names the release binary
key = "myapp.pub"      # optional path, required for self-update
revision = 7           # optional release number; the runtime refuses non-newer downloads
embed = true           # optional, false for an external updater
unsigned = false       # optional, true accepts updates with no signature check
```

When present, onelf selects the update-capable runtime, much larger
than slim (sizes in the
[Self-Update size table](../guide/self-update.md#size-tradeoff)). `url`
must be `https://`, or the pack fails. `key` is the 32-byte Ed25519
public key from `onelf key new`, or several concatenated into a keyring,
and the runtime refuses to self-update without it. A file that holds a
secret key is refused. `revision` is embedded as
`.onelf/update-revision` and covered by the signature. At update time the
runtime refuses a download whose revision is not strictly newer than the
running one, and a running package that has a revision refuses a
download without one. `embed = false` records the URL and key but links
the slim runtime, for packages updated by a package manager.
`unsigned = true` is the opt-out from all of that verification: the
package stores no key and embeds a `.onelf/update-unsigned` marker, and
its runtime installs whatever the update URL serves, warning on stderr
every time. HTTPS is still required, and `unsigned` cannot be combined
with `key`. See
[Opting out of signatures](../guide/self-update.md#opting-out-of-signatures).

## `[bundle]`

```toml
[bundle]
lib-dirs = ["auto"]              # default ["auto"]; the bundle's library dirs
search-paths = [
  "${MUSL_LIBDRM}/lib",
  "/opt/custom/lib",
]                                # Vec<String>, supports ${VAR}, relative to the recipe's directory
exclude = ["libpthread"]         # Vec<String>, soname prefixes to skip, plugin members included
include = ["libfoo.so.1"]        # Vec<String>, force-include sonames
gl = false                       # opt-in, never enabled by detection
dri = false                      # opt-in
vulkan = false                   # opt-in
wayland = false                  # also enabled when detected
gtk = false                      # also enabled when detected
no-gl = false                    # force-off, overrides gl
no-dri = false                   # force-off, overrides dri
no-vulkan = false                # force-off, overrides vulkan
no-wayland = false               # force-off, overrides detection and wayland
no-gtk = false                   # force-off, overrides detection and gtk
strip = false                    # run strip --strip-unneeded
strict-libc = false              # skip wrong-family libc libs
scan-dlopen = false              # scan strings for common dlopen'd libs
dlopen = ["libmyvendor.so.1"]    # extra sonames for scan-dlopen allow-list
trace-run = false                # run the command and bundle what it loads at runtime
trace-seconds = 5                # how long the traced run may go
trace-args = ["--version"]       # arguments for the traced run
skip = false                     # no scan or audit: the tree is packed unchecked, with no completeness claim
rules = ["vendor-framework.toml"]  # Vec<PathBuf>, local descriptor files, relative to the recipe's directory
disable-rules = ["translations"] # Vec<String>, built-in or local rule identities to switch off
```

`rules` names project-local framework descriptors in the built-in
catalog's schema (see [Project-local rules](../guide/bundling#project-local-rules)).
A plugin group in one may list `members`, paths relative to the found
directory, to collect only those. A descriptor's runtime mapping may not
name a loader-control or runtime-owned variable: any `LD_*` name,
`GCONV_PATH`, `GLIBC_TUNABLES` or an `ONELF_*` name is refused. They are
validated together with the built-ins before
the AppDir changes: a local identity may not repeat a built-in one, even
a disabled one, or another local file's, and every `disable-rules` entry
must name a known rule exactly once. Each failure names the file and the
rule. A disabled rule never activates and its runtime mappings are not
carried. The enabled rules' runtime mappings travel in
`.onelf/runtime-rules` (see the [File Format](./file-format#runtime-metadata-records)),
which a build writes only when they differ from the built-ins or a set's
resources were prepared, so the descriptor files are not needed at
launch.

The GPU stack (`gl`, `dri`, `vulkan`) is bundled only when asked for,
because the host's driver is the one that works. Wayland and GTK data
are bundled when the binary needs them, and `no-*` turns either off.
With a `[sysroot]` that names a `path`, the framework bundlers are not
used at all, so setting `gl`, `dri`, `vulkan`, `wayland` or `gtk` is an
error, and `skip` is ignored because the sysroot is applied by
`bundle-libs`. A `[sysroot]` without `path` keeps the host scan, flags
included.

## `[sysroot]`

```toml
[sysroot]
path = "../sysroot"                   # PathBuf, optional; must be outside the AppDir
archive = "../platform-1.tar.zst"     # Option<PathBuf>, materialized into path when absent; a path or an https:// URL
hash = "3f2a..."                       # Option<String>, BLAKE3 of the archive; a mismatch is not unpacked
platform = "platform-1"               # Option<String>, provenance label; defaults to the archive or directory name
optional = ["mesa"]                   # Vec<String>, packages to add to the closure, by name
platform-line = "../platform.txt"     # Option<PathBuf>, soname prefixes the host provides
policy = "../policy.txt"              # Option<PathBuf>, glob patterns that never ship
trace = "../trace.txt"                # Option<PathBuf>, paths a test run opened
keep = "../keep.txt"                  # Option<PathBuf>, glob patterns the trace may not prune
platform-url = "https://..."          # Option<String>, GL build for hosts without one; overrides the sysroot's
platform-hash = "3f1c...a9e2"         # Option<String>, its BLAKE3 hash; overrides the sysroot's
```

With `path`, the bundle is the closure of the entrypoint's package in
that rootfs' package database. Without `path` there is no database: the
AppDir is the one the author assembled (an upstream release unpacked
into place, say), bundling runs its ordinary host scan unless `[bundle]
skip` is set, and the section only records the sets and an explicit GL
build pin for the runtime to fetch at launch. The verifier reads each
set's pinned build, fetching an `https://` one into the platform store
when it is not there, to attribute what it ships. That mode accepts nothing else:
`archive`, `hash`, `platform`, `optional`, `platform-line`, `policy`,
`trace` and `keep` are database concepts, and naming one without `path`
is a recipe error, as is a `[sysroot]` with neither `path` nor sets nor
a GL build pin. See
[Sets without a sysroot](../guide/sysroot#sets-without-a-sysroot).

`platform-url` and `platform-hash` override what the sysroot's
`etc/onelf/platform.toml` names, field by field; both are required
together, with or without `path`. A URL is `https://` or `file://`. An
`archive` URL must be `https://`.

A build with `[sysroot]` pins every entry's mtime to 0 unless
`[package].mtime` or `SOURCE_DATE_EPOCH` is set, so the package depends
on the archive and the recipe alone. See [Pinning a GL build](../guide/sysroot#pinning-a-gl-build-for-hosts-without-one).

### `[sysroot.sets.<name>]`

```toml
[sysroot.sets.qt]
packages = ["qt6-base", "qt6-declarative"]   # Vec<String>, sysroot packages whose closure the set holds
url = "https://mirror.example.org/qt-6.7.onelf"  # String, https:// or file://
blake3 = "3f1c...a9e2"                       # String, the set's BLAKE3
```

A dependency set shared between packages. The closure of `packages` is
left out of the bundle and expected from the build at `url`, made with
`onelf sysroot pack-set`. `<name>` is letters, digits, `-` and `_` only. See [Sharing a dependency set](../guide/sysroot#sharing-a-dependency-set)
and [Sets without a sysroot](../guide/sysroot#sets-without-a-sysroot),
which pins a set with no `path` at all. `packages` is required either
way; without a sysroot it is a note on what the set was made from, since
no database is consulted.

Which sets an entrypoint uses is decided by the optional `sets` lists
in `[package]` and in each `[[entrypoint]]`. The semantics are exact:

- An **omitted list selects every declared set**, for the package
  command and for each named entrypoint independently. A named
  entrypoint's omission does not inherit the command's explicit list.
- **`sets = []` selects none**: the launch fetches, mounts and
  extracts nothing, and does not depend on any declared set.
- **A written list selects exactly its names**, read in canonical
  (sorted) declaration order whatever order they were written in.
- A repeated name, and a name the recipe does not declare, fail when
  the recipe loads, naming the list they were written in.
- A `[package]` list beside a `default = true` entrypoint fails when the
  recipe loads: that entrypoint replaces the package command in the
  package, so write the list as its own `sets`.

A recipe with any explicit list, empty or not, is nonlegacy: it packs
the selections as `.onelf/entrypoint-sets` and refuses a runtime
substitution that does not advertise payload metadata and
selection-view support. With every list omitted no record is written,
which the runtime reads as every declared set, and the package stays
byte-identical to what the recipe always packed. See
[Selecting dependency sets](../guide/recipe#selecting-dependency-sets).

## `[paths]`

```toml
[paths]
"/usr/share/galculator" = "share/galculator"
```

Absolute path the application opens, mapped to the directory in the
bundle that should answer for it. Applied in a private mount namespace
before `exec`, so it is visible to the application and its children and
to nothing else. For an application that compiled a data path into
itself and offers no environment variable; prefer `[env]` where one
exists. See [Environment](../guide/environment#making-the-bundle-answer-for-a-compiled-in-path).

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
replace.

Expansion timing matters for defaults: at pack time only plain `${VAR}`
of a variable the packer's environment actually has is substituted.
`${VAR:-word}` is left literal by the packer and evaluated by the
runtime just before `exec`, against the live environment, with POSIX
semantics (`word` applies when `VAR` is unset **or empty**). Write
defaults in the `$${VAR:-word}` escaped form unless you want them
resolved on the build machine.

`PATH` defaults to `${ONELF_DIR}/bin:$${PATH:-/usr/bin:/bin}` (the
package's `bin/` prepended, re-exec-safe; falls back to `/usr/bin:/bin`
when the inherited PATH is empty, since onelf setting `PATH` suppresses
glibc's `_CS_PATH` fallback) unless `[env]` sets `PATH` explicitly, in
which case that value is used verbatim (`PATH = "$${PATH}"` opts out of
the prefix).

❌ `LD_LIBRARY_PATH`, `LD_LIBRARY_PATH_32`, `LD_LIBRARY_PATH_64` and
`LD_AOUT_LIBRARY_PATH` are refused as `[env]` keys at load, with
guidance: the loader search path is what onelf resolves, not a
configurable one. Reach libraries with `[bundle] search-paths`, sets,
or the resolver's host policy instead. A build names the key and the
reason: `error: ./onelf.toml: [env] LD_LIBRARY_PATH: refused: the
loader search path is what onelf resolves, not a configurable one`.

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
