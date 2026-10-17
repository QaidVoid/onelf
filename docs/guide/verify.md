# Integrity Verification

`onelf verify` recomputes content hashes in a packed binary and checks
them against the manifest.

The rules this page encodes:

- ✅ Verify every artifact in CI before it ships. Exit code `0` means
  every file entry matched; `1` means at least one did not.
- ⚠️ It catches bit-rot and inconsistent edits, but it is not a signature.
  Anyone who rewrites a package can recompute its hashes; see the
  [self-update](./self-update) caveat. For bit-rot and accidental edits
  to the covered files it is exactly what you want.
- ❌ Do not expect directories and symlinks to be covered. They have no
  content to hash and are skipped.
- ✅ A corrupted manifest never reaches `verify`. The manifest is checked
  via XXH32 during the normal load path and fails to parse first.

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

- ❌ Do not count entries without content. Directories and symlinks have
  nothing to hash and are skipped.
- ✅ A corrupted manifest never reaches `verify`. The manifest is checked
  via XXH32 during the normal load path and fails to parse first.

## Typical CI check

✅ Build, then verify, before the artifact ships:

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
