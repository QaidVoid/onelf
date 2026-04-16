# Runtime Flags

Flags consumed by the runtime before the entrypoint sees `argv`. All
start with `--onelf-` and are namespace-safe: the app never has to care.

## Information

| Flag | Action |
|------|--------|
| `--onelf-icon` | Print the bundled icon (PNG) to stdout, then exit |
| `--onelf-desktop` | Print the bundled `.desktop` file to stdout, then exit |

## Self-update

Only available when the package was built with a `[update] url`.

| Flag | Action |
|------|--------|
| `--onelf-check-update` | Print status, exit 0 if up to date, 1 if update available |
| `--onelf-update` | Delta-download + atomically replace self |

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
the corresponding XDG variable at the new directory.

## Execution mode override

Not a flag, but an environment variable: `ONELF_MODE`. See
[Execution Modes](../guide/execution-modes).

## Passing flags through

All other arguments are forwarded to the entrypoint unchanged. If you
need to pass `--onelf-something` through to the app, it would be
shadowed by the runtime. There's no way to escape this currently.
