# Integrity Verification

`onelf verify` recomputes content hashes in a packed binary and checks
them against the manifest. Useful for:

- Detecting bit-rot on long-lived packages.
- Catching tampering (not a signature; see the
  [self-update](./self-update) caveat, but catches unintentional changes).
- Validating download artifacts in CI before shipping.

## Basic usage

```bash
onelf verify myapp.onelf
```

Prints either:

```
Checked 12 file(s)
OK
```

Exit code `0`. Or, on mismatch:

```
Checked 12 file(s)
1 file(s) failed verification:
  bin/myapp: expected 73d6..., got 0c95...
error: verification failed
```

Exit code `1`.

## What gets checked

Every file entry's compressed blocks are decompressed, concatenated, and
re-hashed with BLAKE3. The result must match the `content_hash` field in
the manifest.

Directories and symlinks have no content to hash; they're skipped.

The manifest itself is checked via XXH32 during the normal load path,
so a corrupted manifest will fail to parse before `verify` runs.

## Typical CI check

```yaml
- name: Verify
  run: |
    onelf build
    onelf verify myapp.onelf
```

## Comparison with other tools

| Tool | Purpose |
|------|---------|
| `onelf verify` | Recompute all BLAKE3 hashes, catch any drift |
| `sha256sum myapp.onelf` | Catch changes to the whole file, not individual entries |
| `onelf info` | Print metadata, doesn't check content |
| `onelf list` | Show file tree, doesn't check content |
