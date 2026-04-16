# AppDir Layout

An AppDir is a plain directory tree with a conventional structure. onelf
doesn't enforce it strictly, but bundle-libs and the runtime expect the
following layout:

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
└── .onelf/                      # auto-generated metadata (written by pack)
    ├── interp                   # bundled interpreter path (cross-libc)
    ├── update-url               # self-update zsync URL
    └── package-info.toml        # version, description, license
```

## Conventions

- **`bin/`** is where entrypoints live. The `command` in `onelf.toml`
  refers to a path like `bin/myapp`.
- **`lib/`** is auto-added to `LD_LIBRARY_PATH` by the runtime when any
  `.so` file exists there. Subdirectories like `lib/dri/` trigger special
  handling (`LIBGL_DRIVERS_PATH`).
- **`share/`** is prepended to `XDG_DATA_DIRS` so GLib/GTK discover bundled
  schemas, icons, and mime types.
- **`.onelf/`** is the onelf namespace. Don't put your own files here.

## Minimal AppDir

The absolute minimum is just a single binary under `bin/`:

```
myapp/
└── bin/
    └── myapp
```

If `myapp` is fully static (no `DT_NEEDED` entries), this is enough. For
dynamic binaries you'll need matching `.so` files in `lib/`.

## Nested layouts

onelf handles nested install roots too. If your binary is at
`myapp/usr/bin/myapp` and libs at `myapp/usr/lib/`, that works fine, but
the conventional flat `bin/` + `lib/` layout keeps things simpler.

## Starting from a prefix

If you already have a distro-style prefix install (e.g. from `DESTDIR=...
make install`), point onelf at it:

```bash
onelf bundle-libs /path/to/prefix --target /path/to/prefix/usr/bin/myapp
onelf pack /path/to/prefix -o myapp.onelf --command usr/bin/myapp
```
