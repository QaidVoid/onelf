# Desktop Integration

onelf can ship a `.desktop` entry and icon inside the package and
install them into XDG directories so the app shows up in the desktop
menu.

## Bundling icons and desktop files

Place them under `.onelf/` in your AppDir:

```
myapp/
├── bin/
│   └── myapp
├── lib/
│   └── ...
└── .onelf/
    ├── icons/
    │   ├── myapp.svg          # or myapp.png, default.svg, default.png
    │   └── default.png        # fallback when no entrypoint-specific icon
    └── desktop/
        ├── myapp.desktop      # matched by entrypoint name
        └── default.desktop    # fallback
```

Resolution order for icons (first match wins):

1. `.onelf/icons/{entrypoint}.svg`
2. `.onelf/icons/{entrypoint}.png`
3. `.onelf/icons/default.svg`
4. `.onelf/icons/default.png`

Resolution order for desktop files:

1. `.onelf/desktop/{entrypoint}.desktop`
2. `.onelf/desktop/default.desktop`

::: tip
You can also put icons and desktop files under `share/` for the running
app's own use (e.g. GTK icon theme lookups). The `.onelf/` convention
is specifically for extraction and desktop integration commands.
:::

## Integrate with the desktop

The fastest way to add the app to your menu:

```bash
# Using the onelf CLI tool:
onelf integrate ./myapp.onelf

# Or using the binary itself (no CLI tool needed):
./myapp.onelf --onelf-integrate
```

This installs:
- The icon to `$XDG_DATA_HOME/icons/hicolor/.../apps/onelf-myapp.{svg,png}`
- A `.desktop` file to `$XDG_DATA_HOME/applications/onelf-myapp.desktop`

The `Exec=` and `Icon=` fields are automatically patched to point to
the binary's absolute path and installed icon name.

If the package has no bundled `.desktop` file, a minimal one is
generated with the package name.

## Remove desktop integration

```bash
onelf unintegrate ./myapp.onelf

# Or:
./myapp.onelf --onelf-unintegrate
```

## Selecting an entrypoint

For packages with multiple entrypoints:

```bash
onelf integrate ./myapp.onelf --entrypoint myapp-daemon
```

## Extract without installing

To extract the raw icon or desktop file without installing them:

```bash
# Icon to stdout or file
onelf icon myapp.onelf
onelf icon myapp.onelf -o myapp.png

# Desktop file to stdout or file
onelf desktop myapp.onelf
onelf desktop myapp.onelf -o myapp.desktop
```

Runtime-side equivalents:

```bash
./myapp.onelf --onelf-icon > myapp.png
./myapp.onelf --onelf-desktop > myapp.desktop
```

## Desktop file tips

The `.desktop` file should reference the packed binary:

```ini
[Desktop Entry]
Name=MyApp
Exec=myapp %f
Icon=myapp
Type=Application
Categories=Utility;
```

When you run `onelf integrate`, the `Exec=` line is rewritten to the
binary's absolute path automatically. The `Icon=` field is set to the
installed icon's theme name.

If you have multiple entrypoints, use their argv[0] names:

```ini
Exec=myapp-daemon
```

combined with the matching symlink `myapp-daemon -> myapp.onelf`.
