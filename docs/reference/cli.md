# CLI Reference

All commands accept `--help` for usage.

## `onelf init`

Scaffold a starter `onelf.toml`.

```
onelf init [-o FILE] [--binary PATH] [--force]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-o`, `--output` | `onelf.toml` | Where to write the recipe |
| `--binary` | none | Seed name/command from this binary's basename |
| `--force` | `false` | Overwrite existing file |

## `onelf build`

Run bundle-libs + pack from a recipe.

```
onelf build [PATH] [-o FILE]
```

| Arg / Flag | Default | Description |
|------------|---------|-------------|
| `PATH` | `.` | Directory or `.toml` file |
| `-o`, `--output` | `[package] output`, else `<name>.onelf` next to the recipe | Override recipe's output path |

A recipe with a `[sysroot]` pins every entry's mtime to `0` unless it
sets `[package] mtime` or `SOURCE_DATE_EPOCH` is set. See
[Reproducible Builds](../guide/reproducible).

## `onelf run`

Run an AppDir in place for dev iteration.

```
onelf run [PATH] [--command PATH] [--entrypoint NAME] [--bundle] [-- ARGS...]
```

| Flag | Description |
|------|-------------|
| `PATH` | AppDir or `.toml` file (default `.`) |
| `--command` | Binary to exec, relative to AppDir |
| `--entrypoint` | Select a recipe-defined entrypoint |
| `--bundle` | Run `bundle-libs` from the recipe first |
| `-- ARGS` | Passed to the entrypoint |

## `onelf pack`

Pack a directory into an executable.

```
onelf pack DIRECTORY -o OUTPUT --command PATH [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-o`, `--output` | required | Output file |
| `--command` | required | Path to main binary within DIRECTORY |
| `--name` | command basename | Package name |
| `--entrypoint NAME=PATH` | | Add extra entrypoint (repeatable) |
| `--default-entrypoint NAME` | | Select default entrypoint |
| `--lib-dir DIR` | `[auto]` | Library dir of the bundle (repeatable) |
| `--level N` | `19` | Zstd compression level (0 to 22) |
| `--block-size SIZE` | `256K` | Bytes per payload block, `K`/`M` suffix allowed (4K to 32M) |
| `--dict` | `false` | Train shared zstd dictionary |
| `--no-compress` | `false` | Store payload raw, no zstd (overrides `--dict`) |
| `--prefetch FILE` | | Startup profile recorded with `ONELF_PROFILE`. The runtime decompresses the blocks it lists ahead of the app. See [Prefetching a startup](../guide/execution-modes#prefetching-a-startup) |
| `--preload PATH` | | Library dlopen'd on every exec via onelf-env (repeatable, re-exec-safe) |
| `--memfd` | `false` | Mark the default entrypoint memfd-eligible (conflicts with `--no-memfd`) |
| `--no-memfd` | `false` | Force memfd eligibility off |
| `--cache` | `false` | Let the runtime fall back to the persistent cache when no other execution mode works |
| `--needs-setuid` | `false` | The app runs setuid binaries such as `sudo` or `pkexec`, so keep it out of a user namespace |
| `--mtime SECS` | | Pin every entry's mtime to a Unix timestamp for reproducible output. Overrides `SOURCE_DATE_EPOCH` |
| `--working-dir MODE` | `inherit` | `inherit`, `package`, or `command` |
| `--update-url URL` | | URL of the `.zsync` file for self-update. Must start with `https://`. Selects the update runtime unless `--no-embed-updater` is given |
| `--update-key PATH` | | File with one or more 32-byte Ed25519 public keys. Several concatenated keys form a keyring, and any of them can sign updates. A file that holds a secret key, is not a multiple of 32 bytes or repeats a key is refused. Requires `--update-url`, and self-update needs it |
| `--update-revision N` | | Stamp release number N into the package; the runtime refuses downloads that are not strictly newer. Requires `--update-url` |
| `--no-embed-updater` | `false` | Record the update URL without embedding the updater (keeps the slim runtime; see the self-update guide's size table) |
| `--host-libs MODE` | `auto` | What the launch resolver may take from the host: `never` nothing, `auto` the GPU driver stack's closure, `always` any bundled soname the host has newer |
| `--exclude GLOB` | | Exclude paths matching glob (repeatable) |

## `onelf bundle-libs`

Resolve and copy shared library dependencies.

```
onelf bundle-libs DIRECTORY [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--target PATH` | all ELF | Analyze only this binary |
| `--from-binary PATH` | | Copy binary into `DIRECTORY/bin/` first |
| `--lib-dir DIR` | `lib` | Where to place bundled libs |
| `--exclude PFX` | | Soname prefixes to skip (comma/repeat) |
| `--include SONAME` | | Force-include this soname (comma/repeat) |
| `--search-path DIR` | | Extra lib search dir (highest priority) |
| `--dry-run` | `false` | Report without copying |
| `--no-recursive` | `false` | Don't resolve transitive deps |
| `--gl`, `--dri`, `--vulkan` | `false` | Bundle the GPU stack (Mesa GL/EGL/GBM, DRI drivers, Vulkan ICDs). Opt-in: it is never bundled on detection alone, because the host's driver is the one that works |
| `--wayland`, `--gtk` | auto | Framework bundlers. Auto-detect inspects both `DT_NEEDED` and versioned soname strings in the binary, so dlopen'd frameworks are picked up too |
| `--no-gl`, `--no-dri`, `--no-vulkan`, `--no-wayland`, `--no-gtk` | `false` | Force a framework off, overriding auto-detection and the matching positive flag |
| `--strip` | `false` | Run `strip --strip-unneeded` |
| `--strict-libc` | `false` | Skip wrong-family libc libs |
| `--scan-dlopen` | `false` | Scan binary strings for common dlopen sonames |
| `--dlopen SONAME` | | Extra sonames for `--scan-dlopen` (comma/repeat) |
| `--trace-run` | `false` | Run the `--target` for a few seconds in a sandbox and bundle every library it loaded at runtime, from the glibc loader's own report (a musl target needs `strace`). The GPU driver stack it loads is left to the host. Requires `--target` |
| `--trace-seconds N` | `5` | How long the traced run may go before it is stopped; requires `--trace-run` |
| `--trace-arg ARG` | | Argument for the traced run (repeatable); requires `--trace-run` |
| `--sysroot DIR` | | Take the bundle's contents from this sysroot's package database; needs `--target`. Excludes the framework bundlers `--gl`, `--dri`, `--vulkan`, `--wayland` and `--gtk`, and refuses `--trace-run` |
| `--sysroot-optional PKG` | | Optional dependency to include from the sysroot (repeatable); requires `--sysroot` |
| `--platform-line FILE` | | Soname prefixes the host provides, one per line; requires `--sysroot` |
| `--policy FILE` | | Glob patterns that never ship, one per line; requires `--sysroot` |
| `--trace FILE` | | Paths a test run opened, one per line; requires `--sysroot` |
| `--keep FILE` | | Glob patterns the trace may not prune, one per line; requires `--sysroot` |

## `onelf sysroot`

Obtain and inspect pinned sysroots. See [Bundling from a Sysroot](../guide/sysroot).

```
onelf sysroot fetch SOURCE DIR [--hash BLAKE3]
onelf sysroot info DIR
onelf sysroot pack-gl DIR -o FILE [--sysroot SYSROOT --package NAME...]
onelf sysroot pack-set DIR -o FILE --name NAME [--sysroot SYSROOT --package NAME... [--platform-line FILE]]
```

| Command | Description |
|---------|-------------|
| `fetch SOURCE DIR` | Materialize a `.tar` or `.tar.zst` rootfs from a local path or an `https://` URL into `DIR`. With `--hash`, an archive whose BLAKE3 does not match is not unpacked |
| `info DIR` | Print the package count, file count and glibc version of a materialized sysroot |
| `pack-gl DIR -o FILE` | Pack a GL build for hosts without one and print the BLAKE3 hash to pin in `platform.toml`. With `--sysroot` and `--package`, `DIR` is built first from the named packages' closure: shared objects and driver description directories, glibc left out. Otherwise `DIR` is a tree you built. Either way it is verified to be self-contained |
| `pack-set DIR -o FILE --name NAME` | Pack a shared dependency set and print the BLAKE3 hash to pin in a recipe's `[sysroot.sets.<name>]`. `NAME` is letters, digits, `-` and `_` only, as the recipe requires. With `--sysroot` and `--package`, `DIR` is built first from the named packages' closure, everything but glibc and the development and documentation directories; otherwise `DIR` is a tree you built. `--platform-line` names the soname prefixes the host provides; without it the GPU driver families are the line. Verified to be self-contained like a GL build |

## `onelf info`

Show metadata.

```
onelf info BINARY
```

## `onelf list`

List packaged files.

```
onelf list BINARY
```

## `onelf extract`

Extract files from a packed binary.

```
onelf extract BINARY [-o OUT] [--file PATH ...] [--preserve-mode]
```

Without `--file`, extracts everything to `onelf_extracted/` (or `-o`).
With one `--file` and `-o -`, pipes that file to stdout. File modes are
masked to `0o777` unless `--preserve-mode` is given, which keeps the
setuid, setgid and sticky bits. Do not use it on a package you do not
trust.

A package extracts itself the same way with
[`--onelf-extract`](./runtime-flags#inspecting-the-contents).

## `onelf verify`

Check a packed binary's integrity: the container structure first, then
every file entry.

```
onelf verify BINARY [--key KEYFILE --sig SIGFILE]
```

For format 2 packages the structure check covers the runtime ELF prefix
and the footer, both hashed into the manifest. Format 1 packages carry
no such hashes and are refused with a clear message, since their
structure cannot be checked at all. These hashes detect corruption and
inconsistent edits, but anyone who rewrites a file can recompute them,
so a passing check does not say who produced the file.

The entry sweep recomputes a BLAKE3 for each file and compares it
against the manifest.

With `--key` and `--sig` (always together), also check a detached
Ed25519 signature over the whole file. This is the same check the
runtime's self-update applies before installing. `KEYFILE` is the raw
32-byte public key `onelf key new` writes, or a keyring of several
concatenated keys, any of which may have signed; `SIGFILE` is the raw 64-byte
signature `onelf sign` writes. A signature over a modified file fails,
and only the holder of the secret key can make a new one.

Exit `0` on match, `1` on mismatch.

## `onelf icon`

Extract the bundled icon.

```
onelf icon BINARY [--entrypoint NAME] [-o FILE]
```

## `onelf desktop`

Extract the bundled `.desktop` file.

```
onelf desktop BINARY [--entrypoint NAME] [-o FILE]
```

## `onelf integrate`

Install desktop shortcut and icon for a packed binary.

```
onelf integrate BINARY [--entrypoint NAME]
```

Installs every icon size the package carries to
`$XDG_DATA_HOME/icons/hicolor/` and a `.desktop` file to
`$XDG_DATA_HOME/applications/` (see
[Desktop Integration](../guide/desktop#integrate-with-the-desktop)). Both are named `onelf-<name>`,
or `onelf-<name>-<entrypoint>` for an entrypoint other than the default,
so integrating a second entrypoint does not replace the first. The
`Exec=`, `TryExec=`, and `Icon=` fields are patched automatically, and
so is the `Exec=` of every desktop action. If the package has no bundled
desktop file, a minimal one is generated.

| Flag | Description |
|------|-------------|
| `--entrypoint` | Entrypoint whose icon and desktop file to install (default: the one `BINARY` launches) |

`Exec=` names `BINARY`. When `--entrypoint` names one that `BINARY` would
not start on its own, `Exec=` runs it as
`env ONELF_ENTRYPOINT=<name> BINARY`. See
[Desktop Integration](../guide/desktop#selecting-an-entrypoint).

## `onelf unintegrate`

Remove desktop shortcut and icons installed by `integrate`. Pass the same
`--entrypoint` that was integrated.

```
onelf unintegrate BINARY [--entrypoint NAME]
```

## `onelf key`

Manage the Ed25519 keys that sign self-updates.

```
onelf key new [--secret PATH] [--public PATH]
onelf key show --secret PATH [-o PATH]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--secret PATH` | `onelf.key` | Secret key file, created owner-only |
| `--public PATH` | `onelf.pub` | Public key file, 32 raw bytes for `--update-key` |
| `-o, --output PATH` | stdout | Where `show` writes the public key |

`new` refuses to overwrite an existing key file.

## `onelf sign`

Write the detached signature the runtime verifies before installing an
update.

```
onelf sign BINARY --key PATH [-o PATH]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--key PATH` | | Secret key file from `onelf key new` |
| `-o, --output PATH` | derived from the package's update URL | Signature output path |

The default output name comes from the update URL the package carries,
because that is the only name the runtime requests: a package whose
update URL is `app.onelf.zsync` needs its signature at
`app.onelf.zsync.sig`. Signing refuses when the package embeds update
keys and the secret matches none of them. It warns, and still signs,
when the package embeds no key. See
[Self-Update](../guide/self-update) for the full publish flow.

## `onelf cache`

Manage the persistent cache: packages extracted by the cache mode, and
the GL builds fetched for hosts without one. `list` shows both; `gc`
removes packages and builds unused past the age threshold.

```
onelf cache list
onelf cache clear
onelf cache gc [--max-age DAYS]
```
