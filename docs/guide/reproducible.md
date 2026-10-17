# Reproducible Builds

onelf produces byte-identical output for byte-identical input, with one
caveat: file modification times.

The rules this page encodes:

- ✅ Set `SOURCE_DATE_EPOCH` for every build, or pin `--mtime`. File
  mtimes are the one input that is not deterministic out of the box.
- ☑️ A recipe build from a `[sysroot]` pins mtimes to 0 by default.
- ❌ Different rust toolchains between builds. The toolchain is part of
  the input.
- ❌ Reordering `--search-path` between builds. Same soname in two paths,
  different bundled content.
- ⚠️ `--scan-dlopen` output depends on the host environment.
- ☑️ `ONELF_RT_PATH` skips the runtime build, with ELF/machine-type checks.

## What's deterministic by default

- Directory traversal order (alphabetical, via jwalk's sort)
- Zstd compression output (deterministic for a given level)
- BLAKE3 content hashes, XXH32 manifest checksum
- Package ID (BLAKE3 of the manifest bytes)
- Layout offsets (no random padding)
- Symlink targets (stored as strings, not resolved)

## What isn't, out of the box

The problem is mtime. It comes from the filesystem, so two fresh Git
checkouts on different days pick up "now", and repacking them yields
different hashes even though the content is identical.

❌ Repacking fresh checkouts without fixing the epoch first.

## SOURCE_DATE_EPOCH

✅ Set the
[`SOURCE_DATE_EPOCH`](https://reproducible-builds.org/docs/source-date-epoch/)
environment variable to a Unix timestamp. Mtimes newer than the epoch
are clamped down; older ones keep their original mtime.

```bash
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) onelf build
```

Two builds from the same commit on different machines then produce the
same bytes.

## Pinning every mtime

✅ `--mtime SECS` (or `mtime` under `[package]` in the recipe) stamps
every entry with the same timestamp, whatever the filesystem says. It
overrides `SOURCE_DATE_EPOCH`, and unlike the epoch it also moves files
older than the timestamp.

```bash
onelf pack ./AppDir -o app.onelf --command bin/app --mtime 0
```

☑️ A recipe build with a `[sysroot]` section is reproducible from the
archive and the recipe alone, so `onelf build` pins every mtime to 0
unless the recipe sets `mtime` or `SOURCE_DATE_EPOCH` holds a time. The
time the AppDir happened to be written never reaches the package.

## CI example

```yaml
# .github/workflows/release.yml
- name: Pack
  run: SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) onelf build
```

❌ `github.event.head_commit.timestamp` is an ISO 8601 date, not a Unix
timestamp. onelf warns once about a `SOURCE_DATE_EPOCH` that is not a
whole number of seconds and then treats it as unset.

✅ Any CI that checks out the same commit produces byte-identical output,
which in turn makes the zsync delta size predictable.

## Verifying reproducibility

Build twice and compare:

```bash
SOURCE_DATE_EPOCH=1700000000 onelf build -o a.onelf
SOURCE_DATE_EPOCH=1700000000 onelf build -o b.onelf
sha256sum a.onelf b.onelf
# Hashes should be identical.
```

## The onelf binary itself

Everything above concerns the package you produce. The `onelf` binary
also embeds a runtime, and that runtime is built reproducibly too: the
same source, the same toolchain and a fixed `SOURCE_DATE_EPOCH` give a
byte-identical runtime, and therefore byte-identical packages.

✅ Build paths are a non-issue. The runtime is compiled with the cargo
registry, your home directory, the workspace root and the rustc sysroot
all remapped to stable placeholders, so a checkout at a different path or
a different `CARGO_HOME` still produces the same bytes.

❌ A different rust version compiles different code. Pin the toolchain
alongside everything else to reproduce one machine's bytes on another.

☑️ `ONELF_RT_PATH` pointed at a runtime you built earlier skips the
runtime build entirely. It is checked for ELF magic and for a machine
type matching the target being built, so a stale or foreign artifact
fails the build rather than being silently embedded.

## Known sources of non-determinism you might introduce

- ❌ Different compiler versions between builds (the bundled binaries differ).
- ❌ Libraries resolved from different nix store paths on different machines.
- ⚠️ `--scan-dlopen` output can depend on what `bundle-libs` found in the
  host environment, which may change.
- ❌ `--search-path` ordering matters: putting two paths in different
  orders gives different bundled content when both have the same soname.

✅ For strictest reproducibility, pin everything: rust toolchain, nixpkgs
revision, exact paths for `--search-path`.
