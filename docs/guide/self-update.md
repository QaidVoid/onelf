# Self-Update

onelf integrates [zsync](https://zsync.moria.org.uk/) for delta updates.
After a user has your app, they can pull updates with one command and the
runtime downloads only the differing blocks from your hosted binary.

Updates are signed. The runtime verifies a detached Ed25519 signature
over the fully assembled binary before it replaces anything, and refuses
to update at all if the package carries no key. Publishing an update
therefore takes a keypair, and the key has to be embedded when you pack.

## Generate a signing key

```bash
onelf key new --secret myapp.key --public myapp.pub
```

The secret key is written owner-only. Keep it out of the repository and
back it up: replacing it later breaks self-update for every user already
holding a package built with the old public key, and there is no recovery
short of a manual re-download.

The public key is 32 raw bytes, exactly the form `--update-key` embeds.
If you lose your copy of it, re-derive it from the secret:

```bash
onelf key show --secret myapp.key -o myapp.pub
```

## Opt in at pack time

Set both the update URL and the public key. The URL should point at the
`.zsync` control file you'll host alongside the binary.

```toml
[update]
url = "https://releases.example.com/myapp.onelf.zsync"
key = "myapp.pub"
```

On the CLI that is `--update-url` plus `--update-key`. A package built
with a URL but no key cannot self-update: the runtime reports that
self-update is disabled and does not contact the server.

When `[update]` is present, onelf links the larger update-capable
runtime (1.36 MB extra for the HTTPS, zsync and signature code). Without
it, the slim runtime is used and `--onelf-update` is unrecognized. If
something else updates the package, keep the metadata and drop that code
with `embed = false`, described below.

The URL must be `https://`. A plain-HTTP URL is rejected before any
request is made.

## Publish

The order matters. The signature covers the binary's exact bytes, so it
has to be produced after everything that changes them.

```bash
# 1. Build the binary.
onelf build

# 2. Generate the zsync control file. It describes the binary but does
#    not modify it.
zsyncmake myapp.onelf -u https://releases.example.com/myapp.onelf

# 3. Sign the finished binary.
onelf sign myapp.onelf --key myapp.key
```

`sign` prints where the signature has to go:

```
Signed with the key this package embeds.
Signature: myapp.onelf.zsync.sig
Publish it at: https://releases.example.com/myapp.onelf.zsync.sig
```

Note the name. The runtime builds the signature URL by appending `.sig`
to the **update URL**, which names the zsync control file, so the
signature over `myapp.onelf` is published as `myapp.onelf.zsync.sig`.
`onelf sign` derives that name from the URL inside the package so you do
not have to work it out. A signature published under any other name is
simply never requested, and the update fails closed.

Upload all three: `myapp.onelf`, `myapp.onelf.zsync`, and
`myapp.onelf.zsync.sig`.

`sign` refuses if the secret does not match the public key embedded in
the package. That mismatch is otherwise invisible, since such a package
installs and runs normally and only its update path is dead.

:::info
`zsyncmake` comes from the [zsync-rs](https://crates.io/crates/zsync-rs)
crate. The C `zsyncmake` from most distros works too; they produce the
same format.
:::

## Updating from outside the package

Recording where a package updates from and embedding an updater are two
separate choices. By default an update URL brings the update-capable
runtime with it, which costs 1.36 MB over the slim runtime, making the
runtime 63% larger.

When something else does the updating, that code is dead weight, and
worse than dead weight: a self-update replaces a file the package manager
owns, so its database and the disk stop agreeing about what is installed.

```bash
onelf pack app -o myapp.onelf --command bin/myapp \
  --update-url https://releases.example.com/myapp.onelf.zsync \
  --no-embed-updater
```

or in a recipe:

```toml
[update]
url = "https://releases.example.com/myapp.onelf.zsync"
embed = false
```

The package still records the URL and the signing key, so an external
updater reads them the same way. It just carries no update code, and the
update flags do nothing. Read the metadata with `onelf info`:

```
Update:
  URL:          https://releases.example.com/myapp.onelf.zsync
  Signing key:  embedded
  Updater:      external (this package does not update itself)
```

Signing still applies. An external updater that verifies the signature
gets the same guarantee the embedded one does, using the key from the
package and the `.sig` from the server.

## User-side commands

```bash
./myapp.onelf --onelf-check-update   # print "up to date" or "update available"
./myapp.onelf --onelf-update         # download delta, atomically replace self
```

The update-apply flow:

1. Fetch the `.zsync` control file over HTTPS.
2. Compare it against the running binary.
3. If different, delta-download the new binary, using the current one
   as a seed (most blocks match, so typical downloads are tiny).
4. Fetch the detached signature and verify it over the assembled binary
   against the key embedded at pack time.
5. Only then, atomically `rename(2)` the new file over the old one.

Any failure in step 4 removes the downloaded file and leaves the running
executable untouched.

Exit codes:

| Code | Meaning |
|------|---------|
| 0 | Up to date, or update applied successfully |
| 1 | Update available (for `--onelf-check-update`) |
| 2 | Error (network, parse, write, verify) |

## Example CI workflow

Keep the secret key in your CI secret store base64-encoded, since it is
raw binary, and decode it into a file for the signing step. Never echo it
or commit it.

```yaml
- name: Build
  run: onelf build

- name: Generate .zsync
  run: |
    cargo install zsync-rs
    zsyncmake myapp.onelf -u https://releases.example.com/myapp.onelf

- name: Sign
  env:
    SIGNING_KEY: ${{ secrets.ONELF_SIGNING_KEY }}
  run: |
    printf '%s' "$SIGNING_KEY" | base64 -d > signing.key
    onelf sign myapp.onelf --key signing.key
    rm -f signing.key

- name: Upload
  uses: svenstaro/upload-release-action@v2
  with:
    repo_token: ${{ secrets.GITHUB_TOKEN }}
    file: "myapp.onelf{,.zsync,.zsync.sig}"
    file_glob: true
```

## Size tradeoff

| Runtime | Size | Self-update | Records an update URL |
|---------|------|-------------|----------------------|
| slim | 807 KB | no | no |
| slim, `embed = false` | 807 KB | no | yes |
| update | 2.17 MB | yes | yes |

The default is slim. Take the update runtime only if the package should
update itself; if something else does, the middle row keeps the metadata
without the code.

## Security considerations

- The signature is the security boundary. It is checked over the fully
  assembled binary, against a key baked into the package at pack time,
  before anything is installed.
- zsync's own checksum catches corruption and accidental mismatches
  during the transfer. It is not a signature and is not what makes the
  update safe.
- Compromising your server is not enough to push a malicious update,
  since the attacker would also need the signing key. Compromising the
  signing key is enough, so treat it accordingly.
- Serve everything over HTTPS. The runtime enforces this for every
  request the update makes, so no redirect can downgrade the transfer to
  cleartext. The signature fetch follows no redirects at all; the
  download follows them, since publishing behind a redirecting CDN is
  ordinary and HTTPS plus the signature check keeps it safe.
- Certificates are checked against the machine's own trust store rather
  than a set of roots compiled into the binary. A host that terminates
  TLS at a corporate proxy can update, provided that proxy's CA is
  installed the usual way. A host with no trust store at all cannot
  verify anything, and the update fails closed.
