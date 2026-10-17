# Desktop Integration

onelf can ship a `.desktop` entry and icon inside the package and
install them into XDG directories so the app shows up in the desktop
menu.

The rules this page encodes:

- ✅ Put integration input under `.onelf/`: icons in `.onelf/icons/`,
  desktop files in `.onelf/desktop/`. That is what the extraction and
  desktop integration commands read.
- ✅ Reference the packed binary in the desktop file (`Exec=myapp`,
  `Icon=myapp`). `onelf integrate` rewrites `Exec=` and `TryExec=` to
  the binary's absolute path and sets `Icon=` to the installed icon's
  theme name.
- ✅ Integrate a second entrypoint through its symlink. Its
  `{entrypoint}`-named files win over the `default.*` fallbacks.
- ✅ Name a multi-entrypoint `Exec=` after the entrypoint's argv[0] and
  ship the matching symlink (`myapp-daemon -> myapp.onelf`).
- ☑️ No bundled desktop file: `integrate` generates a minimal one with
  the package name.

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

☑️ Copies under `share/` serve the running app itself (e.g. GTK icon
theme lookups). ✅ For the menu entry and icon, put them under
`.onelf/` instead: that is the location the extraction and desktop
integration commands read.

## Integrate with the desktop

The fastest way to add the app to your menu:

```bash
# Using the onelf CLI tool:
onelf integrate ./myapp.onelf

# Or using the binary itself (no CLI tool needed):
./myapp.onelf --onelf-integrate
```

This installs:
- The icon to `$XDG_DATA_HOME/icons/hicolor/scalable/apps/onelf-myapp.svg`,
  or for a PNG to `hicolor/<W>x<H>/apps/onelf-myapp.png` at the size its
  header gives. An icon that is neither SVG nor a readable PNG is not
  installed.
- A `.desktop` file to `$XDG_DATA_HOME/applications/onelf-myapp.desktop`

`$XDG_DATA_HOME` defaults to `~/.local/share`. `myapp` here is the
package name, with any character other than a letter, digit, `-` or `_`
replaced by `-`. `update-desktop-database` is run afterwards when it is
installed.

The `Exec=`, `TryExec=` and `Icon=` fields are automatically patched to
point to the binary's absolute path and installed icon name.

☑️ A package with no bundled `.desktop` file still integrates: a
minimal one is generated with the package name.

## Remove desktop integration

```bash
onelf unintegrate ./myapp.onelf

# Or:
./myapp.onelf --onelf-unintegrate
```

## Selecting an entrypoint

✅ For a package with multiple entrypoints, integrate through the
entrypoint's symlink:

```bash
ln -s myapp.onelf myapp-daemon
onelf integrate ./myapp-daemon
```

The symlink's name selects the entrypoint, as it does at launch, so that
entrypoint's icon and desktop file resolve. `Exec=` keeps the symlink,
so the menu entry launches that entrypoint. An entrypoint other than the
default installs `onelf-myapp-myapp-daemon.desktop` and its icon beside
the default's files rather than over them, and `onelf unintegrate
./myapp-daemon` removes only those.

⚠️ `--entrypoint` picks which entrypoint's files resolve, but `Exec=`
still names the path you pass. `onelf integrate ./myapp.onelf
--entrypoint myapp-daemon` warns that `myapp.onelf` launches another
entrypoint, and the menu entry would start that one.

⚠️ `./myapp-daemon --onelf-integrate` also selects the entrypoint by
name, but writes the resolved path of `myapp.onelf` into `Exec=`, so
the menu entry starts the default entrypoint. Use `onelf integrate`
through the symlink for a second entrypoint.

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

✅ Reference the packed binary in the `.desktop` file:

```ini
[Desktop Entry]
Name=MyApp
Exec=myapp %f
Icon=myapp
Type=Application
Categories=Utility;
```

When you run `onelf integrate`, the `Exec=` line is rewritten to the
binary's absolute path automatically, keeping any arguments and field
codes after it. The `Icon=` field is set to the installed icon's theme
name.

⚠️ Only the `[Desktop Entry]` group is patched. An `Exec=` inside a
`[Desktop Action ...]` group is left as written.

✅ Multiple entrypoints: use their argv[0] names:

```ini
Exec=myapp-daemon
```

combined with the matching symlink `myapp-daemon -> myapp.onelf`.
