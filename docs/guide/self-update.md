# Self-Update

onelf integrates [zsync](https://zsync.moria.org.uk/) for delta updates.
After a user has your app, they can pull updates with one command and the
runtime downloads only the differing blocks from your hosted binary.

Updates are signed. The runtime verifies a detached Ed25519 signature
over the fully assembled binary before it replaces anything, and refuses
to update at all if the package carries no key. Publishing an update
therefore takes a keypair, and the key has to be embedded when you pack.

The rules this page encodes:

- ✅ Set both the update URL and the public key at pack time. A package
  with a URL but no key cannot self-update: the runtime reports that
  self-update is disabled and does not contact the server.
- ✅ Give each release `--update-revision N`. Without it, the older
  freshness rule applies and a signed-but-old package can roll a client
  back.
- ✅ Sign after `zsyncmake` and publish the signature at the update URL
  plus `.sig`. A signature published under any other name is never
  requested, and the update fails closed.
- ❌ Never publish over plain HTTP. A plain-HTTP URL is rejected before
  any request is made.
- ⚠️ Rotate keys through a keyring. A single-key package signed with a
  new secret deadens self-update for everyone until they manually
  install a package carrying the new key.
- ☑️ `embed = false` when something else updates the package: the update
  code is dead weight, and a self-update would leave the package
  manager's database and the disk disagreeing about what is installed.

## Generate a signing key

```bash
onelf key new --secret myapp.key --public myapp.pub
```

- ✅ Keep the secret key out of the repository and back it up. It is
  written owner-only.
- ⚠️ A single-key package is the fragile special case: replacement
  strands every user holding one. With a keyring (see
  [Rotating the signing key](#rotating-the-signing-key)) replacing the
  key is a normal, recoverable operation.

The public key is 32 raw bytes, exactly the form `--update-key` embeds.
If you lose your copy of it, re-derive it from the secret:

```bash
onelf key show --secret myapp.key -o myapp.pub
```

A key file may also hold several public keys concatenated (a multiple of
32 bytes), forming a keyring: the runtime then accepts an update signed
with *any* key in it. That is the mechanism behind key rotation below.

## Opt in at pack time

Set both the update URL and the public key. The URL should point at the
`.zsync` control file you'll host alongside the binary.

```toml
[update]
url = "https://releases.example.com/myapp.onelf.zsync"
key = "myapp.pub"
```

On the CLI that is `--update-url` plus `--update-key`.

- ❌ Build with a URL but no key. The package cannot self-update: the
  runtime reports that self-update is disabled and does not contact the
  server.
- ✅ Give each release a number with `--update-revision N`. The number is
  embedded in the package as `.onelf/update-revision`, so the signature
  covers it like any other byte. At update time the runtime reads the
  revision out of the downloaded package and refuses to install one that
  is not strictly newer than the revision of the running binary: a
  compromised server can then offer only packages you signed, and a
  signed-but-old package cannot roll a client back.
- ☑️ Omit the revision if the older freshness rule is what you want:
  packages without a revision install whenever the SHA1 differs, so the
  flag is optional.
- ☑️ `embed = false` when something else updates the package. When
  `[update]` is present, onelf links the larger update-capable runtime
  (1.35 MB extra for the HTTPS, zsync and signature code); without it,
  the slim runtime is used and `--onelf-update` is unrecognized. See
  [Updating from outside the package](#updating-from-outside-the-package).
- ❌ A plain-HTTP URL. The URL must be `https://`; it is rejected before
  any request is made.

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

- ⚠️ Note the name. The runtime builds the signature URL by appending
  `.sig` to the **update URL**, which names the zsync control file, so
  the signature over `myapp.onelf` is published as
  `myapp.onelf.zsync.sig`. `onelf sign` derives that name from the URL
  inside the package so you do not have to work it out. ❌ A signature
  published under any other name is never requested, and the update
  fails closed.
- ✅ Upload all three: `myapp.onelf`, `myapp.onelf.zsync`, and
  `myapp.onelf.zsync.sig`.
- ⚠️ `sign` refuses if the secret does not match the public key embedded
  in the package. The mismatch is otherwise invisible: such a package
  installs and runs normally, and only its update path is dead.

## Rotating the signing key

The problem: a public key is baked into every package you have published,
and installed clients verify against the keys they embed, so a new
keypair cannot replace the old one.

- ❌ Signing a single-key package with a new secret. That deadens
  self-update for everyone until they manually install a package
  carrying the new key.
- ✅ Rotation through a keyring, in three steps.

1. **Introduce the new key.** Generate a new keypair, build a keyring
   file holding the old and the new public keys concatenated, pack a
   release with `--update-key myapp-ring.pub`, and sign it with the OLD
   secret. Installed clients verify it against their embedded old key,
   and the ring hands them the new key inside the signature-covered
   body.
2. **Switch the signer.** From the next release on, sign with the new
   secret. Every client that took the ring release verifies it.
3. **Retire the old key.** Drop it from the ring in a later release once
   clients have moved. If the old secret leaked, do this immediately:
   clients that install that release stop trusting the compromised key.

Releases signed with the old secret keep updating clients whose ring
still carries it, so a client that skips a release is never stranded;
only when the key leaves the ring does its signature stop being
accepted. A single-key package has no such grace period.

:::info
`onelf key new` refuses to overwrite an existing key file. That is the
rotation path working: keep both keypairs, build the ring with `cat
myapp.pub myapp-new.pub > myapp-ring.pub`, and use `--key` on `sign` to
choose which secret signs a given release.
:::

`onelf info` shows what a package trusts:

```
Update:
  URL:          https://releases.example.com/myapp.onelf.zsync
  Revision:     12
  Signing key:  keyring of 2 keys
  Updater:      embedded
```

:::info
`zsyncmake` comes from the [zsync-rs](https://crates.io/crates/zsync-rs)
crate. The C `zsyncmake` from most distros works too; they produce the
same format.
:::

## Updating from outside the package

Recording where a package updates from and embedding an updater are two
separate choices. By default an update URL brings the update-capable
runtime with it, which costs 1.35 MB over the slim runtime, making the
runtime 2.5 times larger.

- ❌ Embedding the updater when something else does the updating. The
  code is dead weight, and worse than dead weight: a self-update
  replaces a file the package manager owns, so its database and the
  disk stop agreeing about what is installed.
- ✅ Keep the metadata and drop the code:

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
updater reads them the same way. It carries no update code, and the
update flags do nothing. Read the metadata with `onelf info`:

```
Update:
  URL:          https://releases.example.com/myapp.onelf.zsync
  Signing key:  embedded
  Updater:      external (this package does not update itself)
```

✅ Signing still applies. An external updater that verifies the
signature gets the same guarantee the embedded one does, using the key
from the package and the `.sig` from the server.

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
5. If both the running binary and the download carry a
   `.onelf/update-revision`, refuse unless the download is strictly
   newer. This check sits after the signature, so the revision it reads
   is authenticated.
6. Only then, atomically `rename(2)` the new file over the old one.

Any failure in step 4 removes the downloaded file and leaves the running
executable untouched. Before the rename, the old binary is also copied
to a sibling `<name>.onelf-prev` and forced to the disk as a rollback
point; on the rare failure of the rename itself, that copy is moved back
into place.

Exit codes:

| Code | Meaning |
|------|---------|
| 0 | Up to date, or update applied successfully |
| 1 | Update available (for `--onelf-check-update`) |
| 2 | Error (network, parse, write, verify) |

## Example CI workflow

- ✅ Keep the secret key in your CI secret store base64-encoded, since
  it is raw binary, and decode it into a file for the signing step.
- ❌ Never echo it or commit it.

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
| slim | 896 KB | no | no |
| slim, `embed = false` | 896 KB | no | yes |
| update | 2.22 MB | yes | yes |

Measured on rustc 1.98.1, x86_64 musl, release profile. This table is
the one source of these numbers; other pages link here rather than
restating them.

The default is slim.

- ✅ Take the update runtime only if the package should update itself.
- ☑️ The middle row when something else does the updating: it keeps the
  metadata without the code.

## Security considerations

- ✅ The signature is the security boundary. It is checked over the fully
  assembled binary, against a key baked into the package at pack time,
  before anything is installed.
- ⚠️ zsync's own checksum catches corruption and accidental mismatches
  during the transfer. It is not a signature and is not what makes the
  update safe.
- ⚠️ Compromising your server is not enough to push a malicious update,
  since the attacker would also need a signing key. Compromising a
  signing key is enough, so treat it accordingly, and remove a
  compromised key from the keyring in your next release: clients that
  install it stop trusting the leaked key.
- ✅ Serve everything over HTTPS. The runtime enforces this for every
  request the update makes, so no redirect can downgrade the transfer to
  cleartext. The signature fetch follows no redirects at all; the
  download follows them, since publishing behind a redirecting CDN is
  ordinary and HTTPS plus the signature check keeps it safe.
- ⚠️ Certificates are checked against the machine's own trust store
  rather than a set of roots compiled into the binary. A host that
  terminates TLS at a corporate proxy can update, provided that proxy's
  CA is installed the usual way. A host with no trust store at all
  cannot verify anything, and the update fails closed.
