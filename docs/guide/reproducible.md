# Reproducible Builds

onelf produces byte-identical output for byte-identical input, with one
caveat: file modification times.

## What's deterministic by default

- Directory traversal order (alphabetical, via jwalk's sort)
- Zstd compression output (deterministic for a given level)
- BLAKE3 content hashes, XXH32 manifest checksum
- Package ID (BLAKE3 of the manifest bytes)
- Layout offsets (no random padding)
- Symlink targets (stored as strings, not resolved)

## What isn't, out of the box

File `mtime` values come from the filesystem. Two fresh Git checkouts on
different days pick up "now" as the mtime, so repacking them yields
different hashes even though the content is identical.

## SOURCE_DATE_EPOCH

Set the [`SOURCE_DATE_EPOCH`](https://reproducible-builds.org/docs/source-date-epoch/)
environment variable to a Unix timestamp. onelf clamps all mtimes to that
value (file mtimes newer than the epoch are clamped down; older ones keep
their original mtime).

```bash
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) onelf build
```

Now two builds from the same commit on different machines produce the
same bytes.

## CI example

```yaml
# .github/workflows/release.yml
- name: Pack
  env:
    SOURCE_DATE_EPOCH: ${{ github.event.head_commit.timestamp || '1' }}
  run: onelf build
```

Any CI that checks out the same commit produces byte-identical output,
which in turn makes the zsync delta size predictable.

## Verifying reproducibility

Build twice and compare:

```bash
SOURCE_DATE_EPOCH=1700000000 onelf build -o a.onelf
SOURCE_DATE_EPOCH=1700000000 onelf build -o b.onelf
sha256sum a.onelf b.onelf
# Hashes should be identical.
```

## Known sources of non-determinism you might introduce

- Different compiler versions between builds (the bundled binaries differ).
- Libraries resolved from different nix store paths on different machines.
- `--scan-dlopen` output can depend on what `bundle-libs` found in the
  host environment, which may change.
- `--search-path` ordering matters: putting two paths in different orders
  gives different bundled content when both have the same soname.

For strictest reproducibility, pin everything: rust toolchain, nixpkgs
revision, exact paths for `--search-path`.
