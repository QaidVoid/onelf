# Portable Directories

A packed onelf binary can ship with its own config, cache, and data
directories. This lets users carry the app on a USB stick (or similar)
without polluting their home directory, like "portable apps" on Windows.

The rules this page encodes:

- ✅ Sibling directories redirect `$HOME` and the XDG variables.
- ❌ An empty file is not a marker. Only a directory redirects its
  variable.
- ⚠️ Ship the directories with the binary. Without them the app runs
  against the real `$HOME`.
- ❌ Expecting `${VAR}` expansion in the env file. Values are
  exported literally.

## How it works

The runtime checks for directories with specific suffixes next to the
packed binary:

| File | Redirects | Original kept at |
|------|-----------|------------------|
| `myapp.onelf.home` | `$HOME` | `$REAL_HOME` |
| `myapp.onelf.config` | `$XDG_CONFIG_HOME` | `$REAL_XDG_CONFIG_HOME` |
| `myapp.onelf.share` | `$XDG_DATA_HOME` | `$REAL_XDG_DATA_HOME` |
| `myapp.onelf.cache` | `$XDG_CACHE_HOME` | `$REAL_XDG_CACHE_HOME` |
| `myapp.onelf.env` (a file) | additional env vars | n/a |

When the runtime finds one of these directories, it points the
corresponding variable at it and keeps the old value in the `REAL_*`
variable. Where an XDG variable was unset, `REAL_*` holds the XDG
default under `$HOME`, such as `$HOME/.config`.

☑️ The name is that of the packed file itself, as the kernel reports
it. A multicall symlink such as `myapp-daemon` uses the directories of
`myapp.onelf`, the file it points to.

## Example

```
~/Applications/
├── myapp.onelf
├── myapp.onelf.home       ← empty dir, the app stores everything here
└── myapp.onelf.env        ← extra env vars (KEY=VALUE per line)
```

Now running `./myapp.onelf`:

- Sees `$HOME` as `~/Applications/myapp.onelf.home/`
- Reads `myapp.onelf.env` and exports each `KEY=VALUE` line

The app's dot-config, dot-cache, etc. all live inside
`~/Applications/myapp.onelf.home/`, unless the session sets
`XDG_CONFIG_HOME` and friends explicitly. Add the matching `.config`,
`.share` and `.cache` directories to redirect those as well. ✅ Copy `myapp.onelf` plus
`myapp.onelf.home` to another machine and the app keeps its state.

## Creating the portable marker

Use the `--onelf-portable-*` flags:

```bash
./myapp.onelf --onelf-portable         # create all four dirs
./myapp.onelf --onelf-portable-home    # just .home
./myapp.onelf --onelf-portable-config  # just .config
./myapp.onelf --onelf-portable-share   # just .share
./myapp.onelf --onelf-portable-cache   # just .cache
```

## env file format

```sh
# myapp.onelf.env
EDITOR=micro
MY_APP_THEME=dark
EXTRA_PATH=${HOME}/bin
```

Leading/trailing whitespace is trimmed. Blank lines and `#` comments
are ignored. A line `unset NAME` removes `NAME` from the environment.
❌ `${VAR}` is not expanded. The `EXTRA_PATH` line above
therefore exports the literal string `${HOME}/bin`.

## Use cases

- ✅ Per-user isolated state on a shared drive, without wrappers.
- ✅ Testing with a fresh `$HOME` without touching the real one.
- ✅ Apps that misbehave when their config already exists.
