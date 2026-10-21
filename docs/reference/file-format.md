# File Format

A packed onelf file is up to five concatenated sections:

```
┌────────────────────────────────────┐  offset 0
│ Runtime (static musl ELF)          │
│  slim, or the larger update build  │
├────────────────────────────────────┤
│ Manifest (zstd-compressed)         │
│  - header (114 bytes)              │
│  - entrypoints                     │
│  - file/dir/symlink entries        │
│  - lib_dir_offsets                 │
│  - string_table                    │
├────────────────────────────────────┤
│ Payload                            │
│  blocks, zstd or raw               │
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
| 8 | 2 | format_version | `2` (format 1 files have `1`) |
| 10 | 2 | flags | bit 0 `HAS_DICT`, bit 1 `MEMFD_HINT`, bit 2 reserved (never set), bit 3 `STORED`, bit 4 `EXTERNAL_UPDATER`, bit 5 `NO_HOST_LIB_DIRS`, bit 6 `HOST_LIBS_ALWAYS`, bit 7 `CACHE_REQUESTED`, bit 8 `CACHE_PREFERRED`, bit 9 `STIPPLE_PAYLOAD`; other bits reserved |
| 12 | 8 | manifest_offset | Absolute file offset of the compressed manifest |
| 20 | 8 | manifest_compressed | Compressed manifest size in bytes |
| 28 | 8 | manifest_original | Uncompressed manifest size |
| 36 | 8 | payload_offset | Absolute offset of payload start |
| 44 | 8 | payload_size | Payload total size |
| 52 | 8 | dict_offset | Dictionary offset (0 if absent) |
| 60 | 4 | dict_size | Dictionary size (0 if absent) |
| 64 | 4 | manifest_checksum | XXH32 of uncompressed manifest, see below |
| 68 | 8 | end_magic | `"FLENONE\0"` |

## Manifest

Compressed with zstd. After decompression:

```
┌────────────────┐
│ Header (114 B) │
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

### Header (114 bytes in version 3)

| Offset | Size | Field |
|--------|------|-------|
| 0 | 2 | manifest_version (currently 3) |
| 2 | 4 | entry_count |
| 6 | 4 | string_table_size |
| 10 | 2 | entrypoint_count |
| 12 | 2 | default_entrypoint (index into entrypoints) |
| 14 | 2 | lib_dir_count |
| 16 | 2 | name_offset (into string_table) |
| 18 | 32 | package_id (BLAKE3 of the serialized manifest with this field and footer_hash zeroed) |
| 50 | 32 | prefix_hash (BLAKE3 over file bytes `[0, manifest_offset)`; zeros in version 1/2) |
| 82 | 32 | footer_hash (BLAKE3 over the footer, see below; zeros in version 1/2) |

Version 1 and 2 headers end at 50 bytes and carry no structure hashes.

### Entrypoint (14 bytes each)

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | name (string_table offset) |
| 4 | 4 | target_entry (index into entries) |
| 8 | 4 | args (string_table offset, 0x1F-separated) |
| 12 | 1 | working_dir (enum) |
| 13 | 1 | flags (bit 0: `MEMFD_ELIGIBLE`) |

### Entry (variable)

Represents a file, directory, or symlink. Fields are serialized in this
order (65-byte header, then the blocks):

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
the `blocks` arrays in the manifest. Entries whose whole-file BLAKE3
matches share one set of blocks: the packer keeps a map from content
hash to block list, and a repeated file points at the first copy rather
than writing its own. Identical content therefore costs once per
package, and pack reports the total under `Dedup:` when it saved
anything. Two entries still get independent block lists in the manifest
when their content differs, even if individual blocks were to match.

Block size defaults to 64 KiB uncompressed and can be set from 4 KiB to
32 MiB with `--block-size` or `[compression] block-size`. Each block is
compressed independently, which lets the runtime decompress them lazily
as the entrypoint reads files.

If a dictionary is present (`dict_size > 0`), each block is decompressed
against that dictionary. The packer trains it only with `--dict`, at up
to 1 MiB.

When the `STORED` footer flag is set (`onelf pack --no-compress` or
`[compression] store = true`), blocks are written raw with no zstd:
`compressed_size == original_size` for every block, and the runtime
reads the bytes verbatim with zero decompression. `STORED` and
`HAS_DICT` are mutually exclusive. The manifest stays zstd-compressed
either way.

### stipplefs payload

When the `STIPPLE_PAYLOAD` footer flag is set (`onelf pack --payload
stipple` or `[compression] payload = "stipple"`), the payload region is a
[stipplefs](https://github.com/QaidVoid/stipplefs) image instead of
separately compressed blocks. The image deduplicates content-defined chunks across
files, compresses many files per block, and runs an x86 branch filter
over executable code, so it comes out smaller than independently
compressed blocks.

The manifest is unchanged in every other respect: every file is listed
with its blocks and hashes. A block then describes a slice of its file
rather than a region of the payload:

```
payload_offset:  u64        (where the slice starts within the file)
compressed_size: u64        (the file's inode in the stipplefs image)
original_size:   u64        (the slice's length, 64 KiB except the last)
content_hash:    [u8; 32]   (BLAKE3 of the slice)
```

A reader opens the image at `payload_offset` for `payload_size` bytes and
reads each slice through it. The slice's BLAKE3 is checked exactly as a
decompressed block's is, so the manifest remains what a package's
integrity rests on, and the image's own XXH3 checksums are a second line.
`--block-size` sets the image's block size, a power of two up to 64 MiB
and 32 MiB unless chosen, while slices stay 64 KiB. `HAS_DICT` is never set: the image trains its
own dictionary for blocks up to 1 MiB. A reader that predates the flag
fails every hash check rather than serving wrong bytes.

## Reading a packed file

In pseudocode:

```
file = open(path)
file.seek(SeekFrom::End(-76))
footer = read 76 bytes, validate magic
file.seek(footer.manifest_offset)
compressed = read footer.manifest_compressed bytes
manifest_bytes = zstd::decompress(compressed)
checksum = version >= 3 ? XXH32(manifest_bytes with footer_hash zeroed)
                         : XXH32(manifest_bytes)
assert checksum == footer.manifest_checksum
manifest = parse(manifest_bytes)
```

Then the manifest tells you how to read entrypoints, files, etc.

## Integrity

- **XXH32** over the uncompressed manifest bytes catches accidental
  corruption of the manifest itself. From footer format 2 / manifest
  version 3 on, the `footer_hash` header field is zeroed for this
  checksum, because that field is pinned by the footer hash instead.
- **BLAKE3** over each file's concatenated content bytes is stored in
  the entry. `onelf verify` recomputes and compares.
- **BLAKE3** over each block's decompressed bytes is stored alongside the
  block from manifest version 2 onward. This is what a random-access
  reader checks, so a single byte can be served and verified without
  reassembling the file it came from.
- **BLAKE3** over the runtime prefix, `prefix_hash` in the manifest from
  version 3 on, covers the executable body in front of the manifest,
  which nothing else hashed before.
- **BLAKE3** over the footer, `footer_hash` in the manifest, covers the
  footer itself with four layout fields zeroed: `manifest_compressed`,
  `manifest_original`, `payload_offset` and `dict_offset`. Those four
  cannot take part in the chain, because the packer learns them only
  after compressing the manifest, and the manifest can only be compressed
  after the footer hash exists. Their honesty is enforced indirectly: a
  bogus region is caught by the bounds checks or by the hashes of what
  the region is supposed to hold.
- The **package_id** (BLAKE3 of the manifest) uniquely identifies the
  package and drives cache-mode directory names.

The three hashes chain: the manifest checksum covers the manifest, and
the manifest carries both `prefix_hash` and `footer_hash`, so replacing
the executable body or rewriting the footer shows up in `onelf verify`.
This is an integrity check, not authentication. No hash in the chain is
keyed, so anyone who rewrites the file can recompute all of them and
produce a consistent package. Only a detached signature over the file
shows who produced it.

## Format compatibility

The footer's `format_version` accepts `1` and `2`; writers emit `2`.
Format 1 means manifest version 1 or 2: no `prefix_hash`, no
`footer_hash`, and no hash covers the container structure.
`onelf verify` refuses such a file with a clear message instead of
printing OK, but it still runs, as it always did.

The manifest version has moved with the footer. Readers accept `1`, `2`
and `3` and reject anything higher with a clear error, so a package
written by an older onelf still runs. Version 2 added the per-block
`content_hash` (24-byte blocks in version 1, 56 bytes from version 2).
Version 3 extended the header from 50 to 114 bytes with the two structure
hashes. onelf writes a version 3 manifest only behind a format 2 footer.

Footer flags are decoded with `from_bits_retain`, so a bit a reader does
not recognise is kept and ignored rather than treated as corruption. That
is what lets a flag be added without invalidating existing packages, and
why each newer one is set only for the new behaviour: a package written
before they existed reads as "embedded updater, host drivers reachable
through the resolver, no cache unless asked", which is what it was built
expecting. The two host-library bits encode three policies: bit 5 set is
`never`, bit 6 set is `always`, and neither is `auto`. Bit 5 wins when
both are set.
