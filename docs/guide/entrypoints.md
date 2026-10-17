# Entrypoints

A packed binary can expose multiple entrypoints, each invoking the same
or different internal binaries with different args. This is useful for:

- A single app that has a CLI mode and a daemon mode.
- A package that exposes multiple tools (like BusyBox).
- A GUI/TUI split where the same binary takes different flags.

The rules this page encodes:

- ✅ Invoke a non-default entrypoint via a symlink named after it;
  selection is `argv[0]` matching at runtime.
- ✅ Trust memfd auto-detection for a truly static binary. Set `memfd =
  false` only when it gets something wrong.
- ⚠️ `memfd` covers the default entrypoint only. Extra entrypoints
  always start at the FUSE rung of the ladder.
- ✅ `working-dir = "package"` for apps that read relative paths,
  `"command"` for apps that look for siblings, `inherit` otherwise.

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

The default entrypoint is named after the command's file name, `myapp`
here. `onelf info` shows them:

```
Entrypoints:
  myapp (default): bin/myapp
  myapp-daemon: bin/myapp args=--daemon
  myapp-cli: bin/myapp args=--cli
```

## Invoking an entrypoint

The runtime picks the entrypoint in this order:

1. `ONELF_ENTRYPOINT`, when it names an entrypoint of this package and
   was not inherited from a runtime (see below).
2. The file name of `argv[0]`, when it names an entrypoint.
3. The default entrypoint.

✅ The simplest way to select one is a symlink:

```bash
./myapp.onelf             # runs the default entrypoint
ln -s myapp.onelf myapp-daemon
./myapp-daemon            # runs 'myapp-daemon' with --daemon injected
```

This pattern is called **multicall**, and it's how BusyBox and some
toolkits ship many tools in one binary.

```bash
ONELF_ENTRYPOINT=myapp-cli ./myapp.onelf   # select without a symlink
```

☑️ The runtime sets `ONELF_ENTRYPOINT` for the app it starts and records
the same name in `ONELF_ENTRYPOINT_EXPORTED`. A package the app launches,
or the app re-running itself through another symlink, ignores an
`ONELF_ENTRYPOINT` equal to that record, so an inherited name never
overrides `argv[0]`. Setting it to anything else still selects.

## Memfd eligibility per entrypoint

- ✅ Do nothing by default. If the entrypoint's binary has no
  `DT_NEEDED` entries (truly static), onelf auto-sets the memfd flag.
- ☑️ Set `memfd = false` to force it off, if auto-detection gets
  something wrong.

You can mark the default entrypoint as memfd-eligible in the recipe:

```toml
[package]
command = "bin/static-tool"
memfd = true
```

⚠️ The memfd flag only matters for the default entrypoint. Extra
entrypoints always start at the FUSE rung. See
[Execution Modes](./execution-modes#mode-1-memfd-fastest-most-limited)
for what memfd mode leaves out.

## Working directory

The `[package] working-dir` setting (`--working-dir` on `onelf pack`)
controls what cwd every entrypoint sees:

| Value | Cwd |
|-------|-----|
| `inherit` (default) | Whatever directory the user launched from |
| `package` | The package root, `$ONELF_DIR` |
| `command` | The directory containing the entrypoint binary |

- ✅ `package` for apps that expect resources at relative paths like
  `./share/myapp/data`.
- ✅ `command` for apps that look for siblings next to themselves.
- ✅ Leave `inherit` otherwise.

`$ONELF_LAUNCH_DIR` is always set to the caller's original working
directory regardless of the `working-dir` setting, in case the app
needs to recover it.
