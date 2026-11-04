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
| `--binary` | none | Seed name/command from this binary's basename, and write `[input] binary` naming it, so the scaffold builds as it stands |
| `--force` | `false` | Overwrite existing file |

## `onelf build`

Assemble an AppDir from its recipe and pack it. One command resolves
the whole recipe: `[input]` imports a bare binary to the command's
path, a `[sysroot]` applies its package database or records its sets,
the framework rules run over the chosen source, and the packer writes
the resolved set selections. An unresolved required dependency fails
the build whether the contents come from a `[sysroot]` or from a scan of
this machine. `[bundle] skip` runs no audit at all and packs the tree
as unchecked, with no completeness claim.

```
onelf build [PATH] [-o FILE] [--dry-run]
```

| Arg / Flag | Default | Description |
|------------|---------|-------------|
| `PATH` | `.` | Directory or `.toml` file |
| `-o`, `--output` | `[package] output`, else `<name>.onelf` next to the recipe | Override recipe's output path |
| `--dry-run` | `false` | Report what the build would collect, import and emit, with its completeness findings, without writing to the AppDir, fetching anything or running the application |

The build ends its bundling with one sorted block of completeness
findings, headed `Completeness findings (N):`. Each line names the
entrypoint when the finding is specific to one, the framework rule
when one is involved, the object, the outcome and the reason, and the
soname that activated the rule. The outcomes are `selected` (the
source or provider that answers for the object), `unverified` (a
pinned set that was not obtained), `optional omission`, `removed` (by
the policy, a trace or a host package), `unresolved`, `required
failure` and `unchecked` (a compiled-in path or traced run the
assembly could not check).

A dry run uses the same planning and reports the same findings. A
binary import is placed into a private scratch copy, and a sysroot
build populates one, so the AppDir is left exactly as it was. A pinned
set whose build is not already verified on this machine is reported
as `unverified` and satisfies nothing, since a dry run never fetches.
A sysroot that is not materialized yet stops the dry run, which
neither fetches nor unpacks its archive. A traced run is reported as
not exercised. The dry run exits nonzero when it finds a required
failure. Because the drop pass audits the finished tree, a dry run
reports an unresolved need itself rather than the optional objects a
build would omit for it.

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
| `--bundle` | Assemble from the recipe the way a build does (sysroot, sets, rules), acquire the selected entrypoint's sets, then run from the prepared development view |
| `-- ARGS` | Passed to the entrypoint after its own args. Name the PATH explicitly when passing args, so the first one is not read as the path: `onelf run . -- ARGS` |

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
| `--block-size SIZE` | `64K`, `32M` for stipplefs | Bytes per payload block, `K`/`M` suffix allowed (4K to 32M; a stipplefs payload takes a power of two up to 64M) |
| `--payload FORMAT` | `blocks` | `blocks` for files cut into separately compressed blocks, `stipple` for a [stipplefs](../reference/file-format#stipplefs-payload) image, deduplicated and compressed across files |
| `--dict` | `false` | Train shared zstd dictionary |
| `--no-compress` | `false` | Store payload raw, no zstd (overrides `--dict`) |
| `--prefetch FILE` | | Startup profile recorded with `ONELF_PROFILE`. The runtime decompresses the blocks it lists ahead of the app. See [Prefetching a startup](../guide/execution-modes#prefetching-a-startup) |
| `--preload PATH` | | Library dlopen'd on every exec via onelf-env (repeatable, re-exec-safe) |
| `--memfd` | `false` | Mark the default entrypoint memfd-eligible (conflicts with `--no-memfd`) |
| `--no-memfd` | `false` | Force memfd eligibility off |
| `--cache` | `false` | Let the runtime fall back to the persistent cache when no other execution mode works |
| `--prefer-cache` | `false` | Run from the persistent cache before any other execution mode, for an app that records its own path in files that outlive it. Implies `--cache` |
| `--needs-setuid` | `false` | The app runs setuid binaries such as `sudo` or `pkexec`, so keep it out of a user namespace |
| `--mtime SECS` | | Pin every entry's mtime to a Unix timestamp for reproducible output. Overrides `SOURCE_DATE_EPOCH` |
| `--working-dir MODE` | `inherit` | `inherit`, `package`, or `command` |
| `--update-url URL` | | URL of the release binary for self-update. Must start with `https://`. Selects the update runtime unless `--no-embed-updater` is given |
| `--update-key PATH` | | File with one or more 32-byte Ed25519 public keys. Several concatenated keys form a keyring, and any of them can sign updates. A file that holds a secret key, is not a multiple of 32 bytes or repeats a key is refused. Requires `--update-url`, and self-update needs it |
| `--update-revision N` | | Stamp release number N into the package; the runtime refuses downloads that are not strictly newer. Requires `--update-url` |
| `--update-unsigned` | `false` | Accept unsigned updates: store no signing key and mark the package so its runtime installs whatever the update URL serves, warning on stderr. HTTPS is still required. Requires `--update-url`, conflicts with `--update-key` |
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
| `--dry-run` | `false` | Report without copying, fetching or running the target; a `--sysroot` dry run populates a private scratch copy instead of `DIRECTORY` |
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
| `pack-set DIR -o FILE --name NAME` | Pack a shared dependency set and print the BLAKE3 hash to pin in a recipe's `[sysroot.sets.<name>]`. `NAME` is letters, digits, `-` and `_` only, as the recipe requires. With `--sysroot` and `--package`, `DIR` is built first from the named packages' closure, everything but glibc and the development and documentation directories; otherwise `DIR` is a tree you built. `--platform-line` names the soname prefixes the host provides; without it the GPU driver families are the line. Verified to be self-contained like a GL build, and packed beside `FILE` and only renamed onto it once its provider index reads back as verified, so a failed check leaves `FILE` as it was |

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

Integrating the default entrypoint integrates the package: the default
and every other entrypoint that carries a desktop file of its own,
`.onelf/desktop/<entrypoint>.desktop`, each with its own files. An
entrypoint served only by `default.desktop` gets no menu entry.

| Flag | Description |
|------|-------------|
| `--entrypoint` | Entrypoint to integrate alone (default: the one `BINARY` launches; the default entrypoint integrates the package) |

`Exec=` names `BINARY`. When `--entrypoint` names one that `BINARY` would
not start on its own, `Exec=` runs it as
`env ONELF_ENTRYPOINT=<name> BINARY`. See
[Desktop Integration](../guide/desktop#selecting-an-entrypoint).

## `onelf unintegrate`

Remove desktop shortcut and icons installed by `integrate`. Through the
default entrypoint it removes every entrypoint's files; `--entrypoint`
naming another removes only that one's.

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
update URL is `app.onelf` needs its signature at `app.onelf.sig`.
Signing refuses when the package embeds update
keys and the secret matches none of them. It warns, and still signs,
when the package embeds no key. See
[Self-Update](../guide/self-update) for the full publish flow.

## `onelf cache`

Manage the persistent cache: packages extracted by the cache mode, the
GL builds and dependency sets fetched for hosts without them, and the
execution views under `views/`, both the packed launches' and the
development runs'. `list` shows the builds and packages; `gc` removes
packages, builds and views unused past the age threshold, a view only
once no launch holds its lease.

The cache lives under `$XDG_CACHE_HOME/onelf`, else `~/.cache/onelf`.
Run as root, the CLI ignores both variables and keeps a persistent,
root-owned `0700` cache at `/var/cache/onelf`. Only when `/var/cache`
is not a directory root alone may write does it fall back to the
runtime's private base (`/run/onelf`, else `/tmp/onelf-0`), which does
not survive a reboot.

```
onelf cache list
onelf cache clear
onelf cache gc [--max-age DAYS]
```
