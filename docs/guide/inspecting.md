# Inspecting Packages

A packed onelf binary is self-describing. Several commands read its
manifest without executing anything.

## `onelf info`

```bash
onelf info myapp.onelf
```

Sample output:

```
onelf binary: myapp.onelf

Format version: 1
Flags:          Flags { bits: 0 }

Manifest:
  Offset:       679936
  Compressed:   855 bytes
  Original:     1709 bytes
  Checksum:     4889cff3

Payload:
  Offset:       680791
  Size:         2486629 bytes

Package ID:     8d9633d24a4c94b05c6808f971976e6753b371414cac906d22fe02ed7d7dc618
Entries:        13 (4 dirs, 8 files, 1 symlinks)

Metadata:
  name = "myapp"
  version = "0.11.3"
  description = "Example tool"
  license = "MIT"

Entrypoints:
  myapp (default) [memfd]: bin/myapp

Total original size:     5708607 bytes (5.4 MB)
Total compressed size:   2486629 bytes (2.4 MB)
Compression ratio:       43.6%
```

Useful for:
- Checking format version and flags.
- Seeing the package ID (useful for debugging cache entries).
- Listing entrypoints and their memfd eligibility.
- Reading embedded metadata.

## `onelf list`

```bash
onelf list myapp.onelf
```

Shows the file tree with per-entry sizes and hashes:

```
MODE  TYPE  ORIGINAL  COMPRESSED  HASH                             PATH
----  ----  --------  ----------  -------------------------------  ----
755   dir   -         -           -                                bin
755   file  1792536   674354      73d67865...                      bin/myapp
755   dir   -         -           -                                lib
755   file  867264    450913      938c1272...                      lib/libc.musl-x86_64.so.1
...
```

## `onelf extract`

Pull the whole package out to a directory:

```bash
onelf extract myapp.onelf -o extracted/
```

Or pull a single file to stdout:

```bash
onelf extract myapp.onelf --file bin/myapp -o -
```

## `onelf desktop` and `onelf icon`

Extract bundled desktop metadata:

```bash
onelf desktop myapp.onelf    # prints the .desktop file
onelf icon myapp.onelf        # prints a PNG (use -o to save)
```

## `onelf verify`

Recompute BLAKE3 hashes and compare against the manifest. See
[Integrity Verification](./verify).

```bash
onelf verify myapp.onelf
```

## Looking at the live mount

Once you've run a packed binary, the FUSE mount is **invisible** to
other processes on the host because it's in a private mount namespace.

To peek while the app is running:

```bash
# From the running process's perspective (no permissions needed):
ls /proc/$(pgrep myapp.onelf)/root/run/user/$UID/onelf-myapp-*/
```

Or enter its namespace explicitly (requires root):

```bash
sudo nsenter -t $(pgrep myapp.onelf) -m ls /run/user/$UID/onelf-myapp-*/
```

But for 99% of cases, `onelf extract` is the right tool: it unpacks the
package as a regular directory you can browse normally.
