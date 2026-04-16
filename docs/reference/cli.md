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
| `-o`, `--output` | `[package.output]` | Override recipe's output path |

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
| `--lib-dir DIR` | `[auto]` | Library dir for `LD_LIBRARY_PATH` (repeatable) |
| `--level N` | `12` | Zstd compression level (0 to 22) |
| `--dict` | `false` | Train shared zstd dictionary |
| `--memfd` | auto | Force memfd eligibility on |
| `--no-memfd` | | Force memfd eligibility off |
| `--working-dir MODE` | `inherit` | `inherit`, `package`, or `command` |
| `--update-url URL` | | zsync URL; enables update runtime |
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
| `--gl`, `--dri`, `--vulkan`, `--wayland`, `--gtk` | auto | Framework bundlers |
| `--strip` | `false` | Run `strip --strip-unneeded` |
| `--strict-libc` | `false` | Skip wrong-family libc libs |
| `--scan-dlopen` | `false` | Scan binary strings for common dlopen sonames |
| `--dlopen SONAME` | | Extra sonames for `--scan-dlopen` (comma/repeat) |

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
onelf extract BINARY [-o OUT] [--file PATH ...]
```

Without `--file`, extracts everything to `onelf_extracted/` (or `-o`).
With one `--file` and `-o -`, pipes that file to stdout.

## `onelf verify`

Recompute BLAKE3 of each file entry and compare against the manifest.

```
onelf verify BINARY
```

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

## `onelf cache`

Manage the persistent cache (used only by the final-fallback cache mode).

```
onelf cache list
onelf cache clear
onelf cache gc [--max-age DAYS]
```
