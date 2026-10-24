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
- ✅ Give each entrypoint that belongs in the menu a desktop file of
  its own, `.onelf/desktop/{entrypoint}.desktop`. Integrating the
  package then installs all of them in one step.
- ✅ Integrate a single entrypoint through its symlink. Its
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
theme lookups). ✅ For the menu entry, put the desktop file under
`.onelf/` instead: that is the location the extraction and desktop
integration commands read. Integration also installs the app's theme
icons at every size it ships, as described below.

## Integrate with the desktop

The fastest way to add the app to your menu:

```bash
# Using the onelf CLI tool:
onelf integrate ./myapp.onelf

# Or using the binary itself (no CLI tool needed):
./myapp.onelf --onelf-integrate
```

This installs:
- Every icon size the package carries into
  `$XDG_DATA_HOME/icons/hicolor/`, each named `onelf-myapp`:
  - each `icons/hicolor/<size>/apps/<icon>.png` or `.svg` the package
    ships anywhere in its tree, at that `<size>`. `<icon>` is the bundled
    desktop file's `Icon=` name, or failing that the entrypoint or
    package name.
  - the `.onelf/icons` icon, an SVG to `scalable/` and a PNG at the
    `<W>x<H>` its header gives. It replaces a theme icon of that size.

  An icon that is neither SVG nor a readable PNG is not installed.
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

This removes the desktop file and the icon at every size.

## Packages with several menu entries

Integrating through the default entrypoint, by running the package
itself or `onelf integrate` on it, integrates the whole package. That is
the default entrypoint and every other entrypoint that carries a desktop
file of its own, each installed as if integrated separately:

```
.onelf/desktop/myapp.desktop            ->  onelf-myapp.desktop
.onelf/desktop/myapp-settings.desktop   ->  onelf-myapp-myapp-settings.desktop
```

An entrypoint served only by `default.desktop`, such as a command line
tool, gets no menu entry. Unintegrating through the default entrypoint
removes the files of every entrypoint.

## Selecting an entrypoint

✅ To integrate one entrypoint alone, integrate through the
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

✅ Without a symlink, name the entrypoint:

```bash
onelf integrate ./myapp.onelf --entrypoint myapp-daemon
```

`myapp.onelf` on its own starts the default entrypoint, so `Exec=`
names the one you chose in the launch:

```
Exec=env ONELF_ENTRYPOINT=myapp-daemon /path/to/myapp.onelf %U
```

`TryExec=` stays the package path. `./myapp-daemon --onelf-integrate`
writes the same line, since the runtime integrates from the package
file it resolved the symlink to.

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

✅ Actions work the same way. The `Exec=` of every `[Desktop Action ...]`
group is rewritten to launch the package the same way as the entry's,
and the action's own arguments are kept:

```ini
[Desktop Action new-window]
Name=New Window
Exec=myapp --new-window
```

becomes `Exec=/path/to/myapp.onelf --new-window`.

✅ Multiple entrypoints: use their argv[0] names:

```ini
Exec=myapp-daemon
```

combined with the matching symlink `myapp-daemon -> myapp.onelf`.
