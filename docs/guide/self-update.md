# Self-Update

onelf integrates [zsync](https://zsync.moria.org.uk/) for delta updates.
After a user has your app, they can pull updates with one command and the
runtime downloads only the differing blocks from your hosted binary.

## Opt in at pack time

Set `[update] url` in the recipe (or `--update-url` on the CLI). The URL
should point at the `.zsync` control file you'll host alongside the
binary.

```toml
[update]
url = "https://releases.example.com/myapp.onelf.zsync"
```

When `[update]` is present, onelf links the larger update-capable
runtime (about 1.3 MB extra for the HTTPS / zsync code). Without it,
the slim runtime is used and `--onelf-update` is unrecognized.

## Produce the `.zsync` file

After `onelf build`, generate the zsync control file:

```bash
zsyncmake myapp.onelf -u https://releases.example.com/myapp.onelf
```

The `-u` URL points at the actual binary (not the `.zsync`). Upload both
`myapp.onelf` and `myapp.onelf.zsync` to your host.

:::info
`zsyncmake` comes from the [zsync-rs](https://crates.io/crates/zsync-rs)
crate. The C `zsyncmake` from most distros works too; they produce the
same format.
:::

## User-side commands

```bash
./myapp.onelf --onelf-check-update   # print "up to date" or "update available"
./myapp.onelf --onelf-update         # download delta, atomically replace self
```

The update-apply flow:

1. Fetch the `.zsync` control file over HTTPS.
2. Compare its SHA-1 against the running binary.
3. If different, delta-download the new binary, using the current one
   as a seed (most blocks match, so typical downloads are tiny).
4. Atomically `rename(2)` the new file over the old one.

Exit codes:

| Code | Meaning |
|------|---------|
| 0 | Up to date, or update applied successfully |
| 1 | Update available (for `--onelf-check-update`) |
| 2 | Error (network, parse, write, verify) |

## Example CI workflow

```yaml
- name: Build
  run: onelf build

- name: Generate .zsync
  run: |
    cargo install zsync-rs
    zsyncmake myapp.onelf -u https://releases.example.com/myapp.onelf

- name: Upload
  uses: svenstaro/upload-release-action@v2
  with:
    repo_token: ${{ secrets.GITHUB_TOKEN }}
    file: "myapp.onelf{,.zsync}"
    file_glob: true
```

## Size tradeoff

| Runtime | Size | Self-update |
|---------|------|-------------|
| slim | ~700 KB | no |
| update | ~2 MB | yes |

Pick update only if you actually want self-update. The default is slim.

## Security considerations

- The update is verified by zsync's SHA-1 at the end of the download.
- Always serve the `.zsync` and binary over HTTPS.
- The SHA-1 check catches corruption and accidental mismatches. It's
  **not** a signature. Anyone able to modify both files on your host can
  push malicious updates. Consider signed packages (planned) or pinning
  the binary via external means if that matters.
