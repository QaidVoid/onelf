# Integrity Verification

`onelf verify` checks a packed binary against the hashes in its own
manifest: first the container structure, then every file entry.

The rules this page encodes:

- ✅ Verify every artifact in CI before it ships. Exit code `0` means the
  structure and every file entry matched. `1` means something did not.
- ⚠️ It catches bit-rot and inconsistent edits, but it is not a signature.
  No hash in the package is keyed, so anyone who rewrites a package can
  recompute all of them. To check who produced a file, pass `--key` and
  `--sig` (see [Checking a signature](#checking-a-signature)).
- ❌ Do not expect directories and symlinks to be covered. They have no
  content to hash and are skipped.
- ✅ A corrupted manifest never reaches `verify`. The manifest is checked
  via XXH32 during the normal load path and fails to parse first.
- ⚠️ Packages written in format 1 by an older onelf are refused. They
  carry no hash over the runtime or the footer, so a passing result would
  be a lie. Repack them with a current onelf. They still run.

## Basic usage

```bash
onelf verify myapp.onelf
```

Prints either:

```
Structure: OK (format 2, prefix and footer intact)
Checked 12 file(s)
OK
```

Exit code `0`. Or, on a file mismatch:

```
Structure: OK (format 2, prefix and footer intact)
Checked 12 file(s)
1 file(s) failed verification:
  bin/myapp: expected 73d6..., got 0c95...
error: verification failed
```

Exit code `1`.

## What gets checked

1. **The structure.** The runtime ELF in front of the manifest and the
   footer at the end are hashed with BLAKE3 and compared against the
   `prefix_hash` and `footer_hash` the manifest carries. A mismatch stops
   the run with an error naming the part that changed.
2. **Every file entry.** Its blocks are decompressed, each block is
   checked against its own hash, and the concatenated content is
   re-hashed with BLAKE3. The result must match the entry's
   `content_hash`. Failures are collected and listed together.

The [file format reference](../reference/file-format#integrity) describes
how these hashes chain.

## Checking a signature

```bash
onelf verify myapp.onelf --key myapp.pub --sig myapp.onelf.zsync.sig
```

With `--key` and `--sig` (always together), `verify` first checks a
detached Ed25519 signature over the whole file. This is the same check
the runtime's self-update applies before installing. `--key` is the raw
32-byte public key `onelf key new` writes, or a keyring of several
concatenated keys, any of which may have signed. `--sig` is the raw
64-byte signature `onelf sign` writes. On success it prints
`Signature: OK` before the structure line. A signature over a modified
file fails, and only the holder of the secret key can make a new one.

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
| `onelf verify --key --sig` | Also prove who produced the file |
| `sha256sum myapp.onelf` | Catch changes to the whole file, not individual entries |
| `onelf info` | Print metadata, doesn't check content |
| `onelf list` | Show file tree, doesn't check content |
