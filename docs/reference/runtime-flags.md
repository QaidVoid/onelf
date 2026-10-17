# Runtime Flags

Flags the runtime acts on instead of starting the entrypoint. All start
with `--onelf-`. The runtime looks for each one anywhere on the command
line, performs the action and exits without running the app. Icons,
desktop files and integration apply to the entrypoint the launch
selected (see [Entrypoints](../guide/entrypoints)).

## Information

| Flag | Action |
|------|--------|
| `--onelf-icon` | Print the bundled icon (SVG or PNG) to stdout, then exit |
| `--onelf-desktop` | Print the bundled `.desktop` file to stdout, then exit |

## Desktop integration

| Flag | Action |
|------|--------|
| `--onelf-integrate` | Install every icon size and the `.desktop` file to XDG directories, then exit |
| `--onelf-unintegrate` | Remove the icons and `.desktop` file installed by integrate, then exit |

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
