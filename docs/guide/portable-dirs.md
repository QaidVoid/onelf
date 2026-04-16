# Portable Directories

A packed onelf binary can ship with its own config, cache, and data
directories. This lets users carry the app on a USB stick (or similar)
without polluting their home directory, like "portable apps" on Windows.

## How it works

The runtime checks for files with specific suffixes next to the packed
binary:

| File | Redirects | Original kept at |
|------|-----------|------------------|
| `myapp.onelf.home` | `$HOME` | `$REAL_HOME` |
| `myapp.onelf.config` | `$XDG_CONFIG_HOME` | `$REAL_XDG_CONFIG_HOME` |
| `myapp.onelf.share` | `$XDG_DATA_HOME` | `$REAL_XDG_DATA_HOME` |
| `myapp.onelf.cache` | `$XDG_CACHE_HOME` | `$REAL_XDG_CACHE_HOME` |
| `myapp.onelf.env` | additional env vars | n/a |

The file can be empty (just a marker) or can be a directory. When the
runtime finds any of these, it points the corresponding XDG variable at
the sibling file/directory.

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
`~/Applications/myapp.onelf.home/`. You can copy `myapp.onelf` plus
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

Leading/trailing whitespace is trimmed. Blank lines and `#` comments are
ignored. `${VAR}` is not expanded; literal values only.

## Use cases

- Distributing a tool on a shared drive where each user wants isolated
  state without shelling out to wrappers.
- Testing app behavior with a fresh `$HOME` without touching the real one.
- Apps that misbehave when their config already exists.
