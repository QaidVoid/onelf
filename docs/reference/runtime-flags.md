# Runtime Flags

Flags the runtime acts on instead of starting the entrypoint. All start
with `--onelf-`. The runtime looks for each one anywhere on the command
line, performs the action and exits without running the app. Icons,
desktop files and integration apply to the entrypoint the launch
selected (see [Entrypoints](../guide/entrypoints)).

## Information

| Flag | Action |
|------|--------|
| `--onelf-help` | List the runtime flags, then exit |
| `--onelf-version` | Print the runtime version, then the package name and version, then exit |
| `--onelf-icon` | Print the bundled icon (SVG or PNG) to stdout, then exit |
| `--onelf-desktop` | Print the bundled `.desktop` file to stdout, then exit |

The package version is the recipe's `[package] version`, so a package
packed without one prints its name alone.

## Diagnosis

| Flag | Action |
|------|--------|
| `--onelf-doctor` | Print what a launch would do on this host, without running the app, then exit |

The report covers:

- **Host**: whether user namespaces are usable and which mount helper
  would be used, if any.
- **Execution mode**: each mode in the order a launch tries them, with
  the reason a mode would be skipped, down to the one that would run.
  `ONELF_MODE`, `ONELF_CACHE` and `ONELF_FUSE_NO_NAMESPACE` are taken
  into account.
- **Libraries**: the `host-libs` policy, whether the GL stack is bundled,
  from the host or missing, any pinned GL build and whether it is
  stored, and the resolver's verdict for every library it compares: the
  bundled copy, the host's newer one, or incomparable. Libraries only
  the host has are listed together. The host loader and the link farm
  are named when the host's glibc or any host library would be used.
- **Dependency sets**: each pinned set and whether it is stored.
- **Self-update**: the update URL, the number of signing keys, the
  revision, and whether this runtime includes the updater.

Nothing is fetched, mounted, or written to the cache or the resolver's
store. To read the bundled libraries the doctor extracts the library
directories to a scratch directory in the private runtime directory and
removes it before it exits. The libraries of a pinned GL build or a
dependency set are not part of the verdicts, since weighing them would
mean fetching or extracting them.

## Inspecting the contents

| Flag | Action |
|------|--------|
| `--onelf-extract [DIR] [PATTERN...]` | Extract the package to `DIR` (default `onelf_extracted`), or only the paths matching a `PATTERN`, then exit |
| `--onelf-mount` | Mount the package, print the mount point to stdout, and serve it until interrupted |

`--onelf-extract` takes the arguments after it: the first is `DIR`,
every further one a `PATTERN`. A pattern names a path in the package
or a directory above it (`share/doc` extracts everything below it), and
`*` matches any run of characters, `/` included (`'lib/*.so*'`). A
pattern that matches nothing is an error. Every file is checked against
its recorded hash before it is written, as `onelf extract` does, and
file modes are masked to `0o777`.

`--onelf-mount` mounts through `fusermount3` when it can, so every
process of the user sees the mount at the printed path under the
private runtime directory. Without a helper it mounts in a user
namespace of its own, and the printed path reaches it through
`/proc/<pid>/root`. Ctrl-C, `SIGTERM` or `SIGHUP` unmounts it and
removes the directory.

## Desktop integration

| Flag | Action |
|------|--------|
| `--onelf-integrate` | Install every icon size and the `.desktop` file to XDG directories, then exit. Through the default entrypoint this covers every entrypoint with a desktop file of its own |
| `--onelf-unintegrate` | Remove the icons and `.desktop` file installed by integrate, then exit. Through the default entrypoint this removes every entrypoint's files |

## Self-update

Only available when the package embeds the updater: it was built with
an `[update] url` and without `embed = false` (`--no-embed-updater`).
The slim runtime does not know these flags and passes them to the app.

| Flag | Action |
|------|--------|
| `--onelf-check-update` | Print status, exit 0 if up to date or if `--onelf-update` would refuse what the server has, 1 if an update is available, 2 on error |
| `--onelf-update` | Delta-download, verify the signature, and atomically replace self. Refused when the package embeds no update key |

## Portable directory setup

Create sibling directories/files so the runtime treats this as a
portable install.

| Flag | Creates |
|------|---------|
| `--onelf-portable` | All four: `.home`, `.config`, `.share`, `.cache` |
| `--onelf-portable-home` | `<binary>.home/` |
| `--onelf-portable-config` | `<binary>.config/` |
| `--onelf-portable-share` | `<binary>.share/` |
| `--onelf-portable-cache` | `<binary>.cache/` |

After running one of these, future invocations of the binary redirect
the corresponding variable at the new directory. `<binary>` is the
name of the packed file itself, so a multicall symlink uses the
directories of the file it points to. See
[Portable Directories](../guide/portable-dirs).

## Execution mode override

Not a flag, but an environment variable: `ONELF_MODE`, with `memfd`,
`fuse`, `tmpfs`, `rundir` or `cache`. `ONELF_CACHE=1` allows the cache
without forcing it. See [Execution Modes](../guide/execution-modes).

## Passing flags through

All other arguments are forwarded to the entrypoint unchanged,
including any `--onelf-*` argument not listed on this page. An argument
that matches a flag above is always taken by the runtime, and there is
no way to escape it.
