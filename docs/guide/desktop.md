# Desktop Integration

onelf can ship a `.desktop` entry and icon inside the package and expose
them so users can pin the app to their menu or dock.

## Bundling a desktop entry

Put the standard files under `share/` in your AppDir:

```
myapp/
├── bin/
│   └── myapp
├── lib/
│   └── ...
└── share/
    ├── applications/
    │   └── myapp.desktop
    └── icons/
        └── hicolor/
            ├── 128x128/apps/myapp.png
            ├── 256x256/apps/myapp.png
            └── scalable/apps/myapp.svg
```

The runtime automatically prepends `$ONELF_DIR/share` to `$XDG_DATA_DIRS`,
so any GTK/GLib code in the packed app finds the bundled schemas,
icons, and mime types. The user's system tools can also read them
via the extraction commands below.

## Extract the desktop entry

```bash
onelf desktop myapp.onelf                       # print to stdout
onelf desktop myapp.onelf -o myapp.desktop      # write to file
```

The runtime-side equivalent:

```bash
./myapp.onelf --onelf-desktop > myapp.desktop
```

## Extract the icon

```bash
onelf icon myapp.onelf                          # PNG to stdout
onelf icon myapp.onelf -o myapp.png             # to file
```

Runtime-side:

```bash
./myapp.onelf --onelf-icon > myapp.png
```

onelf finds the largest `share/icons/.../apps/<name>.png` it can.

## Installing system-wide

A minimal install script:

```bash
#!/bin/sh
set -eu
APP=./myapp.onelf
DEST="$HOME/.local"

mkdir -p "$DEST/bin" "$DEST/share/applications" "$DEST/share/icons/hicolor/256x256/apps"
cp "$APP" "$DEST/bin/"
"$APP" --onelf-desktop > "$DEST/share/applications/myapp.desktop"
"$APP" --onelf-icon > "$DEST/share/icons/hicolor/256x256/apps/myapp.png"
update-desktop-database "$DEST/share/applications" 2>/dev/null || true
```

Now the user can launch the app from their menu.

## Exec= line

The `.desktop` file should use the packed binary itself:

```ini
[Desktop Entry]
Name=MyApp
Exec=myapp.onelf %f
Icon=myapp
Type=Application
Categories=Utility;
```

If you have multiple entrypoints, use their argv[0] names:

```ini
Exec=myapp.onelf-daemon
```

combined with the matching symlink `myapp.onelf-daemon -> myapp.onelf`.

## Auto-updating menu entries on new installs

Call the install script from `--onelf-update` via a wrapper, or have a
`post-install` hook that sets up the menu entries. onelf doesn't
automate desktop integration on the user's system because that's a
distro-specific concern.
