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

Format version: 2
Flags:          Flags(MEMFD_HINT | NO_HOST_LIB_DIRS)
Host libs:      never
Cache:          only on request

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

Provenance:
  none recorded (built from a host scan)

Entrypoints:
  myapp (default) [memfd]: bin/myapp

Total original size:     5708607 bytes (5.4 MB)
Total compressed size:   2486629 bytes (2.4 MB)
Compression ratio:       43.6%
```

Useful for:
- Checking format version and flags.
- Seeing what the launch resolver may take from the host, and whether
  the package asked for the persistent cache.
- Seeing the package ID (useful for debugging cache entries).
- Listing entrypoints and their memfd eligibility.
- Reading embedded metadata and provenance.
- Checking the update URL, revision and signing key, which an `Update:`
  section shows for a package that has an update URL.

## `onelf list`

```bash
onelf list myapp.onelf
```

Shows every entry with its mode, sizes and full BLAKE3 content hash,
and each symlink with its target (hashes shortened here):

```
MODE       TYPE    ORIGINAL     COMPRESSED   HASH         PATH
---------------------------------------------------------------------
755        dir     -            -            -            bin
755        file    1792536      674354       73d67865...  bin/myapp
755        dir     -            -            -            lib
755        file    867264       450913       938c1272...  lib/libc.musl-x86_64.so.1
777        link    -            -            -            lib/ld-musl-x86_64.so.1 -> libc.musl-x86_64.so.1
...
```

## `onelf extract`

Pull the whole package out to a directory:

```bash
onelf extract myapp.onelf -o extracted/
```

Without `-o` it extracts into `onelf_extracted/`. Modes are masked to
`0o777` unless you pass `--preserve-mode`, which keeps setuid, setgid
and sticky bits and is unsafe for a package you do not trust.

Or pull a single file to stdout:

```bash
onelf extract myapp.onelf --file bin/myapp -o -
```

## `onelf desktop` and `onelf icon`

Extract bundled desktop metadata:

```bash
onelf desktop myapp.onelf    # prints the .desktop file
onelf icon myapp.onelf        # prints the SVG or PNG icon (use -o to save)
```

Both take `--entrypoint NAME` and default to the default entrypoint.

## `onelf verify`

Check the container structure, then recompute every file's BLAKE3
hash and compare it against the manifest. See
[Integrity Verification](./verify).

```bash
onelf verify myapp.onelf
```

## Looking at the live mount

Once you've run a packed binary, the FUSE mount is **invisible** to
other processes on the host because it's in a private mount namespace.

The mount lives in the private runtime directory, `$XDG_RUNTIME_DIR`
or else `/tmp/onelf-$UID`, as `onelf-<name>-<id>`. To peek while the
app is running, with `PID` the app's process id:

```bash
# From the running process's perspective (same user, no root needed):
ls /proc/$PID/root/run/user/$UID/onelf-myapp-*/
```

Or enter its namespace explicitly (requires root):

```bash
sudo nsenter -t $PID -m ls /run/user/$UID/onelf-myapp-*/
```

But for 99% of cases, `onelf extract` is the right tool: it unpacks the
package as a regular directory you can browse normally.

## Provenance

A package built from a [sysroot](./sysroot) records the platform label
and every package that contributed a file. `onelf info` prints them:

```
Provenance:
  Platform:     platform-1.tar.zst
  glibc 2.44-1
  jq 1.8.2-1
  oniguruma 6.9.10-1
```

A package that pins a GL build or shared dependency sets lists them
under `GL build:` and `Shared sets:` in the same section.

A package built from a host scan says `none recorded`. The record is
display only: nothing the runtime does depends on it, and a record that
does not parse is reported as malformed while the rest of the output
still prints.
