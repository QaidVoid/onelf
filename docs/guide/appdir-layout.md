# AppDir Layout

An AppDir is a plain directory tree with a conventional structure. onelf
doesn't enforce it strictly, but bundle-libs and the runtime expect the
following layout:

The rules this page encodes:

- ✅ Use the conventional flat layout: `bin/` for entrypoints, `lib/`
  for bundled libraries, `share/` for data and icons.
- ✅ Let the runtime wire the environment. It hands every directory
  that holds a `.so` to the loader as the library path, and puts
  `share/` on `XDG_DATA_DIRS`.
- ✅ Minimal AppDir: one binary under `bin/` when it is fully static.
- ☑️ Nested install roots work. The conventional flat `bin/` + `lib/`
  layout keeps things simpler.
- ✅ Point onelf at an existing `DESTDIR=... make install` prefix.

```
myapp/
├── bin/
│   ├── myapp                    # the main executable
│   └── helper                   # extra binaries (optional)
├── lib/
│   ├── libfoo.so.1              # bundled shared libraries
│   ├── ld-musl-x86_64.so.1 -> libc.musl-x86_64.so.1    # symlink alias
│   └── dri/                     # optional: DRI drivers
│       └── radeonsi_dri.so
├── share/                       # optional: data files, icons, desktop entry
│   ├── applications/
│   │   └── myapp.desktop
│   └── icons/
│       └── hicolor/256x256/apps/myapp.png
├── onelf.toml                   # optional: recipe (see Recipe File)
└── .onelf/                      # onelf metadata namespace
    ├── icons/                   # icons for desktop integration
    │   └── default.svg          #   <entrypoint>.svg/.png, then default.svg/.png
    ├── desktop/                 # .desktop files for desktop integration
    │   └── default.desktop      #   <entrypoint>.desktop, then default.desktop
    ├── interp                   # bundled interpreter path (cross-libc)
    ├── env                      # [env] variables and the PATH default
    ├── paths                    # [paths] pairs
    ├── preload                  # libraries the onelf-env constructor dlopens
    ├── needs-setuid             # present when the app runs setuid binaries
    ├── prefetch                 # blocks a startup reads, from a profile
    ├── update-url               # self-update URL (names the release binary)
    ├── update-key               # public keys that may sign updates
    ├── update-revision          # release number for rollback protection
    ├── package-info.toml        # version, description, license
    ├── provenance.toml          # platform and packages, from a sysroot build
    ├── platform                 # the GL build a sysroot build pins
    └── sets                     # the shared sets a sysroot build relies on
```

## Conventions

- **`bin/`** is where entrypoints live. The `command` in `onelf.toml`
  refers to a path like `bin/myapp`.
- **`lib/`** is recorded as a library directory at pack time when any
  `.so` file exists there, and the runtime hands it to the loader as the
  library path. The same goes for any other directory holding a `.so`
  outside `bin/`, `share/` and `etc/`. Subdirectories like `lib/dri/`
  trigger special handling (`LIBGL_DRIVERS_PATH`).
- **`share/`** is prepended to `XDG_DATA_DIRS` so GLib/GTK discover bundled
  schemas, icons, and mime types.
- **`.onelf/`** is the onelf namespace. `onelf pack` writes `interp`,
  `env`, `paths`, `preload`, `needs-setuid`, `prefetch`, the `update-*`
  files and `package-info.toml` from its options and the recipe. A
  [sysroot build](./sysroot) writes `provenance.toml`, which only
  `onelf info` reads, and `platform` and `sets` when it pins a GL build
  or relies on shared sets. `icons/` and `desktop/` are user-provided
  for [desktop integration](./desktop). ❌ Do not put anything else
  there: pack refuses any other file under `.onelf/`.

## Minimal AppDir

The absolute minimum is a single binary under `bin/`:

```
myapp/
└── bin/
    └── myapp
```

✅ This is enough when `myapp` is fully static (no `DT_NEEDED`
entries). A dynamic binary needs matching `.so` files in `lib/`.

## Nested layouts

☑️ onelf handles nested install roots: a binary at
`myapp/usr/bin/myapp` with libs at `myapp/usr/lib/` works.
✅ Keep the conventional flat `bin/` + `lib/` layout; it is simpler.

## Starting from a prefix

If you already have a distro-style prefix install (e.g. from `DESTDIR=...
make install`), point onelf at it:

```bash
onelf bundle-libs /path/to/prefix --target /path/to/prefix/usr/bin/myapp
onelf pack /path/to/prefix -o myapp.onelf --command usr/bin/myapp
```
