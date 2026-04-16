# Entrypoints

A packed binary can expose multiple entrypoints, each invoking the same
or different internal binaries with different args. This is useful for:

- A single app that has a CLI mode and a daemon mode.
- A package that exposes multiple tools (like BusyBox).
- A GUI/TUI split where the same binary takes different flags.

## Defining entrypoints

In `onelf.toml`:

```toml
[package]
name = "myapp"
command = "bin/myapp"              # the default entrypoint's binary

[[entrypoint]]
name = "myapp-daemon"              # extra entrypoint name
path = "bin/myapp"                 # re-use the same binary
args = ["--daemon"]                # always prepend these args

[[entrypoint]]
name = "myapp-cli"
path = "bin/myapp"
args = ["--cli"]
```

`onelf info` shows them:

```
Entrypoints:
  myapp (default): bin/myapp
  myapp-daemon: bin/myapp args=--daemon
  myapp-cli: bin/myapp args=--cli
```

## Invoking an entrypoint

Each entrypoint is selected by matching `argv[0]` at runtime. The
simplest way is via a symlink:

```bash
./myapp.onelf             # runs the default entrypoint
ln -s myapp.onelf myapp-daemon
./myapp-daemon            # runs 'myapp-daemon' with --daemon injected
```

This pattern is called **multicall**, and it's how BusyBox and some
toolkits ship many tools in one binary.

## Memfd eligibility per entrypoint

You can mark the default entrypoint as memfd-eligible in the recipe:

```toml
[package]
command = "bin/static-tool"
memfd = true
```

Auto-detection kicks in by default: if the entrypoint's binary has no
`DT_NEEDED` entries (truly static), onelf auto-sets the memfd flag.
Force it off with `memfd = false` if auto-detection gets something wrong.

The memfd flag only matters for the default entrypoint. Extra
entrypoints always go through the normal FUSE/tmpfs path.

## Working directory

The `[package] working-dir` setting controls what cwd the exec'd
entrypoint sees:

| Value | Cwd |
|-------|-----|
| `inherit` (default) | Whatever directory the user launched from |
| `package` | The AppDir root (the FUSE mount) |
| `command` | The directory containing the entrypoint binary |

Use `package` for apps that expect resources at relative paths like
`./share/myapp/data`. Use `command` for apps that look for siblings next
to themselves. Leave `inherit` otherwise.
