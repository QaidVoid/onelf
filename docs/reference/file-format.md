# File Format

A packed onelf file is up to five concatenated sections:

```
┌────────────────────────────────────┐  offset 0
│ Runtime (static musl ELF)          │
│  670 KB (slim) or ~2 MB (update)   │
├────────────────────────────────────┤
│ Manifest (zstd-compressed)         │
│  - header (50 bytes)               │
│  - entrypoints                     │
│  - file/dir/symlink entries        │
│  - lib_dir_offsets                 │
│  - string_table                    │
├────────────────────────────────────┤
│ Payload                            │
│  256 KB blocks, zstd or raw        │
├────────────────────────────────────┤
│ Dictionary (optional, raw)         │
├────────────────────────────────────┤
│ Footer (76 bytes, fixed)           │  last 76 bytes of file
│  - magic, offsets, checksums       │
└────────────────────────────────────┘
```

When executed, the runtime reads its own tail to discover where the
manifest, payload, and dictionary begin.

## Footer layout

76 bytes at the end of the file. All integers little-endian.

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 8 | magic | `"ONELF\0\x01\x00"` |
| 8 | 2 | format_version | `1` |
| 10 | 2 | flags | bit 0 `HAS_DICT`, bit 1 `MEMFD_HINT`, bit 2 `SHARUN_COMPAT`, bit 3 `STORED`, bit 4 `EXTERNAL_UPDATER`, bit 5 `NO_HOST_LIB_DIRS`; other bits reserved |
| 12 | 8 | manifest_offset | Absolute file offset of the compressed manifest |
| 20 | 8 | manifest_compressed | Compressed manifest size in bytes |
| 28 | 8 | manifest_original | Uncompressed manifest size |
| 36 | 8 | payload_offset | Absolute offset of payload start |
| 44 | 8 | payload_size | Payload total size |
| 52 | 8 | dict_offset | Dictionary offset (0 if absent) |
| 60 | 4 | dict_size | Dictionary size (0 if absent) |
| 64 | 4 | manifest_checksum | XXH32 of uncompressed manifest |
| 68 | 8 | end_magic | `"FLENONE\0"` |

## Manifest

Compressed with zstd. After decompression:

```
┌────────────────┐
│ Header (50 B)  │
├────────────────┤
│ Entrypoints    │  14 bytes each * entrypoint_count
├────────────────┤
│ Entries        │  variable-size records * entry_count
├────────────────┤
│ lib_dir_offsets│  4 bytes each * lib_dir_count
├────────────────┤
│ string_table   │  null-terminated strings
└────────────────┘
```

### Header (50 bytes)

| Offset | Size | Field |
|--------|------|-------|
| 0 | 2 | manifest_version (currently 2) |
| 2 | 4 | entry_count |
| 6 | 4 | string_table_size |
| 10 | 2 | entrypoint_count |
| 12 | 2 | default_entrypoint (index into entrypoints) |
| 14 | 2 | lib_dir_count |
| 16 | 2 | name_offset (into string_table) |
| 18 | 32 | package_id (BLAKE3 of the serialized manifest bytes minus this field) |

### Entrypoint (14 bytes each)

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | name (string_table offset) |
| 4 | 4 | target_entry (index into entries) |
| 8 | 4 | args (string_table offset, 0x1F-separated) |
| 12 | 1 | working_dir (enum) |
| 13 | 1 | flags (bit 0: `MEMFD_ELIGIBLE`) |

### Entry (variable)

Represents a file, directory, or symlink. Fields:

Fields are serialized in this order (65-byte header, then the blocks):

```text
kind:           u8        (0=dir, 1=file, 2=symlink)
parent:         u32       (index into entries, 0xFFFFFFFF for top-level)
name:           u32       (string_table offset)
mode:           u32       (unix mode bits)
mtime_secs:     u64
mtime_nsec:     u32
content_hash:   [u8; 32]  (BLAKE3 of concatenated block contents; zero for non-files)
num_blocks:     u32       (count of block refs that follow; derived from blocks.len())
symlink_target: u32       (string_table offset; 0 unless kind == symlink)
blocks:         [Block; num_blocks]  (file content block refs; empty for non-files)
```

Each `Block` is 56 bytes:

```
payload_offset:  u64
compressed_size: u64
original_size:   u64
content_hash:    [u8; 32]   (BLAKE3 of this block's decompressed bytes)
```

`content_hash` arrived with manifest version 2. It lets a reader verify a
single block without reassembling the entry, which is what allows a large
file to be served a byte at a time rather than buffered whole. In a
version 1 manifest the field is absent and a `Block` is 24 bytes, leaving
the whole-entry hash as the only check.

## Payload

Contiguous sequence of blocks. Block boundaries are defined entirely by
the `blocks` arrays in the manifest. The format allows two entries to
point at the same block, but pack.rs currently emits per-entry block
lists with no content-level deduplication.

Block size defaults to 256 KB uncompressed. Each block is compressed
independently, which lets the runtime decompress them lazily as the
entrypoint reads files.

If a dictionary is present (`dict_size > 0`), each block is decompressed
against that dictionary.

When the `STORED` footer flag is set (`onelf pack --no-compress` or
`[compression] store = true`), blocks are written raw with no zstd:
`compressed_size == original_size` for every block, and the runtime
reads the bytes verbatim with zero decompression. `STORED` and
`HAS_DICT` are mutually exclusive. The manifest stays zstd-compressed
either way.

## Reading a packed file

In pseudocode:

```
file = open(path)
file.seek(SeekFrom::End(-76))
footer = read 76 bytes, validate magic
file.seek(footer.manifest_offset)
compressed = read footer.manifest_compressed bytes
manifest_bytes = zstd::decompress(compressed)
assert XXH32(manifest_bytes) == footer.manifest_checksum
manifest = parse(manifest_bytes)
```

Then the manifest tells you how to read entrypoints, files, etc.

## Integrity

- **XXH32** over the uncompressed manifest bytes catches accidental
  corruption of the manifest itself.
- **BLAKE3** over each file's concatenated content bytes is stored in
  the entry. `onelf verify` recomputes and compares.
- **BLAKE3** over each block's decompressed bytes is stored alongside the
  block from manifest version 2 onward. This is what a random-access
  reader checks, so a single byte can be served and verified without
  reassembling the file it came from.
- The **package_id** (BLAKE3 of the manifest) uniquely identifies the
  package and drives cache-mode directory names.

## Format compatibility

The footer's `format_version` must be `1`. It has not moved.

The manifest version has. Readers accept `1` and `2` and reject anything
higher with a clear error, so a package written by an older onelf still
runs. The only difference between the two is the `content_hash` on each
block, so a version 1 manifest has 24-byte blocks and falls back to the
whole-entry hash for verification.

Footer flags are decoded with `from_bits_retain`, so a bit a reader does
not recognise is kept and ignored rather than treated as corruption. That
is what lets a flag be added without invalidating existing packages, and
why the two most recent ones are set only for the new behaviour: a
package written before they existed reads as "embedded updater, host
library directories exposed", which is what it was built expecting.
