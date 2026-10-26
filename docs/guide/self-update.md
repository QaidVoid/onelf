# Self-Update

onelf packages update themselves straight from the release binary. There
is no control file and no delta tool to run: the update URL names the
binary you already publish, and the runtime downloads only the content
it does not already have, copying the rest from the running executable.

The trick is the package's own manifest. Every block of payload content
carries a BLAKE3 hash, and the manifest is small enough to fetch in one
range request. The runtime fetches a release's footer and manifest with
two small HTTPS range requests, compares the hashes against the running
package's manifest, and knows exactly which byte spans changed. Nothing
about the release needs to be prepared beyond signing it.

Updates are signed. The runtime verifies a detached Ed25519 signature
over the fully assembled binary before it replaces anything, and refuses
to update at all if the package carries no key. Publishing an update
therefore takes a keypair, and the key has to be embedded when you pack.
Signing is the default, not a hard requirement: a package packed with
the unsigned opt-in carries no key and installs whatever its release
server serves, saying so on stderr every time. See
[Opting out of signatures](#opting-out-of-signatures).

The rules this page encodes:

- ✅ Set both the update URL and the public key at pack time. A package
  with a URL but no key cannot self-update unless it was packed with the
  unsigned opt-in: the runtime reports that self-update is disabled and
  does not contact the server.
- ☑️ Accept unsigned updates deliberately with the unsigned opt-in
  (`unsigned = true`, or `--update-unsigned`). Such a package stores no
  key and updates from its release server with no signature check: the
  HTTPS connection is then the only protection, and the runtime says so
  on stderr at every update.
- ✅ Give each release `--update-revision N`. Without it, the older
  freshness rule applies and a signed-but-old package can roll a client
  back.
- ✅ Sign the finished binary and publish the signature at the update
  URL plus `.sig`. A signature published under any other name is never
  requested, and the update fails closed.
- ✅ Host the release on a server that honors range requests. Any
  ordinary static file server does. One that answers a range request
  with a full-body 200 still works: the runtime notices and falls back
  to a whole-file download, which only costs bandwidth.
- ❌ Never publish over plain HTTP. `onelf pack` refuses an update URL
  that is not `https://`, and the runtime refuses one before any request
  is made.
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
The secret key is 64 raw bytes.

- ❌ Never pass the secret key to `--update-key`. It would ship inside
  every package, so `onelf pack` recognises a secret key and refuses it.

If you lose your copy of the public key, re-derive it from the secret:

```bash
onelf key show --secret myapp.key -o myapp.pub
```

A key file may also hold several distinct public keys concatenated (a
multiple of 32 bytes), forming a keyring. The runtime then accepts an
update signed with *any* key in it. That is the mechanism behind key
rotation below. `onelf pack` refuses a key file of any other length, or
one that repeats a key.

## Opt in at pack time

Set both the update URL and the public key. The URL points at the
release binary itself, exactly where you will host it.

```toml
[update]
url = "https://releases.example.com/myapp.onelf"
key = "myapp.pub"
```

On the CLI that is `--update-url` plus `--update-key`.

- ❌ Build with a URL but no key. The package cannot self-update: the
  runtime reports that self-update is disabled and does not contact the
  server. The unsigned opt-in below is the one way to change that.
- ✅ Give each release a number with `--update-revision N`. The number is
  embedded in the package as `.onelf/update-revision`, so the signature
  covers it like any other byte. At update time the runtime reads the
  revision out of the downloaded package and refuses to install one that
  is not strictly newer than the revision of the running binary: a
  compromised server can then offer only packages you signed, and a
  signed-but-old package cannot roll a client back. Once a release
  carries a revision, every later one must carry one too: a client with
  a revision refuses a package without one, since that is what every
  release signed before revisions existed looks like.
- ☑️ Omit the revision if the older freshness rule is what you want:
  packages without a revision install whenever the package id differs,
  so the flag is optional.
- ☑️ `embed = false` when something else updates the package. When
  `[update]` is present, onelf links the larger update-capable runtime
  (1.3 MB extra for the HTTPS client and signature code); without it,
  the slim runtime is used and `--onelf-update` is unrecognized. See
  [Updating from outside the package](#updating-from-outside-the-package).
- ❌ A plain-HTTP URL. The URL must be `https://`. `onelf pack` refuses
  anything else.

## Opting out of signatures

Not every release channel needs signatures: an internal tool whose
release server is already trusted, a build exercised end to end in CI.
The unsigned opt-in marks the package so its runtime updates without any
signature check, and no key is embedded.

```toml
[update]
url = "https://releases.example.com/myapp.onelf"
unsigned = true
```

On the CLI that is `--update-url` plus `--update-unsigned`. The package
carries a `.onelf/update-unsigned` marker, `onelf info` shows the
policy, and `--update-unsigned` cannot be combined with `--update-key`.

- ✅ Keep serving over HTTPS. Without a signature the transport is the
  only thing standing between your users and whoever can answer at the
  update URL.
- ⚠️ Every update and every check prints a warning that the package
  verifies no signatures. That is deliberate: the policy should be
  visible to whoever runs the package.
- ✅ Publishing is one file: the binary alone, since nothing fetches a
  `.sig`. The revision gate, the rollback copy and the atomic install
  all still apply.
- ✅ Integrity checks still compare the runtime prefix and file content
  with the downloaded manifest. Ranged requests retain the probed size
  and use a strong ETag when the server supplies one. A mixed or corrupt
  assembly is retried as a full download and checked again. These checks
  detect corruption; they do not authenticate the release server.
- ❌ Reaching for the opt-in because managing a key seems heavy for a
  public release. A keypair is generated once, and the keyring makes
  replacing it routine; an unsigned release server, once compromised,
  pushes arbitrary code to every client.

## Publish

The order matters. The signature covers the binary's exact bytes, so it
has to be produced after everything that changes them.

```bash
# 1. Build the binary.
onelf build

# 2. Sign the finished binary.
onelf sign myapp.onelf --key myapp.key
```

`sign` prints where the signature has to go:

```
Signed with the key this package embeds.
Signature: myapp.onelf.sig
Publish it at: https://releases.example.com/myapp.onelf.sig
```

- ✅ Upload both files: `myapp.onelf` and `myapp.onelf.sig`. That is the
  whole release. The runtime derives the signature URL by appending
  `.sig` to the update URL inside the package, so ❌ a signature
  published under any other name is never requested, and the update
  fails closed.
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
  URL:          https://releases.example.com/myapp.onelf
  Revision:     12
  Signing key:  keyring of 2 keys
  Updater:      embedded
```

## Updating from outside the package

Recording where a package updates from and embedding an updater are two
separate choices. By default an update URL brings the update-capable
runtime with it, which costs 1.3 MB over the slim runtime, making the
runtime more than twice as large.

- ❌ Embedding the updater when something else does the updating. The
  code is dead weight, and worse than dead weight: a self-update
  replaces a file the package manager owns, so its database and the
  disk stop agreeing about what is installed.
- ✅ Keep the metadata and drop the code:

```bash
onelf pack app -o myapp.onelf --command bin/myapp \
  --update-url https://releases.example.com/myapp.onelf \
  --no-embed-updater
```

or in a recipe:

```toml
[update]
url = "https://releases.example.com/myapp.onelf"
embed = false
```

The package still records the URL and the signing key, so an external
updater reads them the same way. It carries no update code, and the
update flags do nothing. Read the metadata with `onelf info`:

```
Update:
  URL:          https://releases.example.com/myapp.onelf
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

The check fetches the release's footer and manifest with two small range
requests and compares the package id, which covers every content hash
and the runtime prefix: equal ids mean equal packages, so an identical
release costs a few kilobytes. When the running binary carries a
`.onelf/update-revision`, a differing id is not enough, because only the
new package's own bytes say its revision. The check then downloads and
verifies the update the way the apply does (steps 3 to 5 below) and
removes it again. An update the apply would refuse is reported as
`no update offered`, with the reason.

The update-apply flow:

1. Fetch the release's footer, then its manifest, with two small range
   requests.
2. Compare package ids. Equal means "already up to date" with no
   download at all.
3. Delta-assemble the new binary: every payload block whose hash the
   running package also has is copied from the running binary, and only
   the rest is fetched. Typical updates share most of their content, so
   the download is a fraction of the release. Copied blocks are decoded
   and re-hashed as they are written; a block that does not hash back is
   fetched instead, so encoder drift can never corrupt an update.
4. Fetch the detached signature and verify it over the assembled binary
   against the keys embedded at pack time. If any bytes were copied and
   the signature does not verify, the release is downloaded in full and
   verified again: the signature stays the final word, and the copy
   optimization never overrides it. A package packed with the unsigned
   opt-in checks the runtime prefix and every file's content against the
   manifest instead, after the warning on stderr. An integrity failure
   retries a full download, which must pass the same checks.
5. If the running binary carries a `.onelf/update-revision`, refuse
   unless the download carries one too and it is strictly newer. A
   running binary without a revision skips this step. This check sits
   after the signature, so the revision it reads is authenticated.
6. Only then, atomically `rename(2)` the new file over the old one.

Any failure in steps 3 to 5 removes the downloaded file and leaves the
running executable untouched. Before the rename, the old binary is also copied
to a sibling `<name>.onelf-prev` and forced to the disk as a rollback
point; on the rare failure of the rename itself, that copy is moved back
into place.

:::info
Packages with a stipplefs payload (`--payload stipple`) delta-update
only when the image is stored uncompressed (`--no-compress`). A
compressed image re-encodes unchanged content against a dictionary
retrained for every build, so its bytes are not stable across builds and
nothing shorter than the whole payload can be reused; the updater says
so on stderr and downloads in full. The default blocks payload always
delta-updates.
:::

Exit codes:

| Code | Meaning |
|------|---------|
| 0 | Up to date, update applied successfully, or (for `--onelf-check-update`) no update offered |
| 1 | Update available (for `--onelf-check-update`), or the package has no update URL, or it has neither a signing key nor the unsigned opt-in |
| 2 | Error (network, parse, write, verify) |

## Example CI workflow

- ✅ Keep the secret key in your CI secret store base64-encoded, since
  it is raw binary, and decode it into a file for the signing step.
- ❌ Never echo it or commit it.

```yaml
- name: Build
  run: onelf build

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
    file: "myapp.onelf{,.sig}"
    file_glob: true
```

## Size tradeoff

| Runtime | Size | Self-update | Records an update URL |
|---------|------|-------------|----------------------|
| slim | 1156 KB | no | no |
| slim, `embed = false` | 1156 KB | no | yes |
| update | 2472 KB | yes | yes |

Measured on rustc 1.99.0, x86_64 musl, release profile. This table is
the one source of these numbers; other pages link here rather than
restating them.

The default is slim. A package that pins a GL build or a shared set at
an `https://` URL also takes the update runtime, whatever `[update]`
says, because only that runtime carries the HTTPS client that fetches
it. See [Pinning a GL build](./sysroot#pinning-a-gl-build-for-hosts-without-one).

- ✅ Take the update runtime only if the package should update itself.
- ☑️ The middle row when something else does the updating: it keeps the
  metadata without the code.

## Security considerations

- ✅ The signature is the security boundary. It is checked over the fully
  assembled binary, against a key baked into the package at pack time,
  before anything is installed.
- ⚠️ The per-block hashes make the delta copy safe to attempt, not safe
  to trust. Every copied block is re-hashed as it is written, and a
  mismatch downgrades that block to a download; the signature over the
  assembled bytes is what makes the update safe.
- ⚠️ Compromising your server is not enough to push a malicious update,
  since the attacker would also need a signing key. Compromising a
  signing key is enough, so treat it accordingly, and remove a
  compromised key from the keyring in your next release: clients that
  install it stop trusting the leaked key.
- ⚠️ The unsigned opt-in removes that boundary on purpose: anyone who
  can answer at the update URL pushes code to every client of the
  package. It exists for channels where the server is already trusted
  and the key ceremony buys nothing, not as a lighter default, and the
  runtime announces it on stderr at every update.
- ✅ Serve everything over HTTPS. The runtime enforces this for every
  request the update makes, so no redirect can downgrade the transfer to
  cleartext. The signature fetch and the download both follow
  redirects, since release hosts such as GitHub answer every asset with
  a redirect to their CDN, and HTTPS plus the signature check keeps it
  safe.
- ⚠️ Certificates are checked against the machine's own trust store
  rather than a set of roots compiled into the binary. A host that
  terminates TLS at a corporate proxy can update, provided that proxy's
  CA is installed the usual way. A host with no trust store at all
  cannot verify anything, and the update fails closed.
