//! Publisher-side Ed25519 signing for self-update.
//!
//! The runtime verifies a detached signature over the whole downloaded
//! binary against a public key embedded at pack time. Both sides are raw
//! fixed-width encodings: a 32-byte public key, a 64-byte signature. The
//! commands here produce exactly those bytes so no conversion step sits
//! between generating a key and embedding it with `pack --update-key`.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use ed25519_compact::{KeyPair, PublicKey, SecretKey};

use onelf_format::EntryKind;

/// Path inside a package holding the raw update public key.
const EMBEDDED_KEY_PATH: &str = ".onelf/update-key";

/// Generate a keypair, writing the public key where `pack --update-key`
/// can take it directly and the secret key owner-only.
///
/// Refuses to overwrite either file. Replacing a secret that is already
/// embedded in published packages permanently breaks self-update for
/// every user holding one, so clobbering is never the helpful default.
pub fn generate(pub_path: &Path, secret_path: &Path) -> io::Result<()> {
    for path in [pub_path, secret_path] {
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} already exists; refusing to overwrite it, since replacing a \
                     key that is already embedded in published packages breaks \
                     self-update for everyone holding one",
                    path.display()
                ),
            ));
        }
    }

    let kp = KeyPair::generate();

    // Created 0600 rather than chmod-ed afterwards, so the bytes are
    // never briefly readable by another user.
    let mut sk_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(secret_path)?;
    sk_file.write_all(kp.sk.as_ref())?;
    sk_file.sync_all()?;

    let mut pk_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(pub_path)?;
    pk_file.write_all(kp.pk.as_ref())?;
    pk_file.sync_all()?;

    Ok(())
}

/// Read a secret key file, reporting a length mismatch rather than
/// panicking on a truncated or armored file.
fn read_secret(path: &Path) -> io::Result<SecretKey> {
    let bytes = std::fs::read(path)?;
    if bytes.len() != SecretKey::BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not a secret key: expected {} raw bytes, found {}",
                path.display(),
                SecretKey::BYTES,
                bytes.len()
            ),
        ));
    }
    SecretKey::from_slice(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a valid secret key: {e}", path.display()),
        )
    })
}

/// The public key corresponding to a stored secret key.
pub fn public_key_of(secret_path: &Path) -> io::Result<Vec<u8>> {
    Ok(read_secret(secret_path)?.public_key().as_ref().to_vec())
}

/// Path inside a package holding the update URL.
const EMBEDDED_URL_PATH: &str = ".onelf/update-url";

/// The default detached-signature path for `file`.
///
/// The runtime asks for the update URL with `.sig` appended, and the
/// update URL names the zsync control file rather than the binary. So a
/// binary published as `app.onelf` with an update URL of
/// `app.onelf.zsync` needs its signature uploaded as
/// `app.onelf.zsync.sig`, even though the signed bytes are the binary's.
/// Naming the local file after the URL removes the guesswork, since a
/// signature published under any other name is simply never fetched.
///
/// Falls back to `<file>.sig` when the target carries no update URL to
/// derive from.
fn default_sig_path(file: &Path, update_url: Option<&str>) -> PathBuf {
    if let Some(name) = update_url.and_then(onelf_format::update::detached_sig_filename) {
        let dir = file.parent().unwrap_or(Path::new("."));
        return dir.join(name);
    }
    let mut name = file.as_os_str().to_os_string();
    name.push(".sig");
    PathBuf::from(name)
}

/// Outcome of checking the signing key against the target package.
pub enum KeyMatch {
    /// The package embeds a key and it is the counterpart of the secret.
    Matches,
    /// The target embeds no update key, so it cannot self-update at all.
    NoEmbeddedKey,
    /// The target is not an onelf package.
    NotAPackage,
}

/// Read one metadata file out of a package, if it is a package and
/// carries that file.
fn embedded_file(file: &Path, path: &str) -> Option<Vec<u8>> {
    let (footer, manifest) = crate::info::read_footer_and_manifest(file).ok()?;
    let idx = crate::metadata::find_entry_by_path(&manifest, path)?;
    let entry = manifest.entries.get(idx)?;
    if entry.kind != EntryKind::File {
        return None;
    }
    let mut f = File::open(file).ok()?;
    let dict = crate::info::read_dict(&mut f, &footer).ok()?;
    crate::extract::decompress_verified(&mut f, &footer, entry, dict.as_deref()).ok()
}

/// Compare the package's embedded update key against the signing key.
///
/// Signing with the wrong key is the one mistake nothing else catches:
/// the package packs, installs and runs normally, and only the update
/// path is dead, with no local symptom for the publisher.
fn check_key_against_package(file: &Path, public: &PublicKey) -> io::Result<KeyMatch> {
    let is_package = crate::info::read_footer_and_manifest(file).is_ok();
    if !is_package {
        return Ok(KeyMatch::NotAPackage);
    }
    match embedded_file(file, EMBEDDED_KEY_PATH) {
        None => Ok(KeyMatch::NoEmbeddedKey),
        Some(embedded) if embedded == public.as_ref() => Ok(KeyMatch::Matches),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} embeds a different update key than this secret produces, so the \
                 signature would be rejected by every client. Sign with the secret \
                 matching the key passed to `pack --update-key`, or repack with the \
                 public key for this secret.",
                file.display()
            ),
        )),
    }
}

/// What signing produced, and where it has to be published.
pub struct Signed {
    /// Where the signature was written locally.
    pub path: PathBuf,
    /// What the key check against the package found.
    pub matched: KeyMatch,
    /// The URL the runtime will request the signature from, when the
    /// package carries an update URL to derive it from.
    pub sig_url: Option<String>,
}

/// Sign `file`, writing the detached signature the runtime fetches.
pub fn sign(file: &Path, secret_path: &Path, out: Option<&Path>) -> io::Result<Signed> {
    let secret = read_secret(secret_path)?;
    let matched = check_key_against_package(file, &secret.public_key())?;

    let mut content = Vec::new();
    File::open(file)?.read_to_end(&mut content)?;

    let signature = secret.sign(&content, None);

    let url = embedded_file(file, EMBEDDED_URL_PATH)
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string());
    let sig_path = out
        .map(PathBuf::from)
        .unwrap_or_else(|| default_sig_path(file, url.as_deref()));

    let mut sig_file = File::create(&sig_path)?;
    sig_file.write_all(signature.as_ref())?;
    sig_file.sync_all()?;

    Ok(Signed {
        path: sig_path,
        matched,
        sig_url: url.as_deref().map(onelf_format::update::detached_sig_url),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("onelf-sign-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generated_keys_have_the_widths_the_runtime_decodes() {
        let d = tmpdir("widths");
        let pk = d.join("k.pub");
        let sk = d.join("k.key");
        generate(&pk, &sk).unwrap();

        assert_eq!(std::fs::read(&pk).unwrap().len(), PublicKey::BYTES);
        assert_eq!(std::fs::read(&sk).unwrap().len(), SecretKey::BYTES);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_secret_key_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let d = tmpdir("mode");
        let pk = d.join("k.pub");
        let sk = d.join("k.key");
        generate(&pk, &sk).unwrap();

        let mode = std::fs::metadata(&sk).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret key must not be readable by others");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn generation_refuses_to_clobber_an_existing_key() {
        let d = tmpdir("clobber");
        let pk = d.join("k.pub");
        let sk = d.join("k.key");
        generate(&pk, &sk).unwrap();
        let original = std::fs::read(&sk).unwrap();

        let err = generate(&pk, &sk).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&sk).unwrap(),
            original,
            "the existing secret must survive a refused generation"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn public_key_round_trips_through_the_secret() {
        let d = tmpdir("derive");
        let pk = d.join("k.pub");
        let sk = d.join("k.key");
        generate(&pk, &sk).unwrap();

        assert_eq!(public_key_of(&sk).unwrap(), std::fs::read(&pk).unwrap());

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_signature_verifies_and_stops_verifying_after_an_edit() {
        let d = tmpdir("verify");
        let pk_path = d.join("k.pub");
        let sk_path = d.join("k.key");
        generate(&pk_path, &sk_path).unwrap();

        let target = d.join("payload.bin");
        std::fs::write(&target, b"the bytes that get signed").unwrap();
        let signed = sign(&target, &sk_path, None).unwrap();
        let sig_path = signed.path;
        assert_eq!(sig_path, d.join("payload.bin.sig"));

        let sig_bytes = std::fs::read(&sig_path).unwrap();
        assert_eq!(sig_bytes.len(), ed25519_compact::Signature::BYTES);

        let pk = PublicKey::from_slice(&std::fs::read(&pk_path).unwrap()).unwrap();
        let sig = ed25519_compact::Signature::from_slice(&sig_bytes).unwrap();
        assert!(pk.verify(std::fs::read(&target).unwrap(), &sig).is_ok());

        std::fs::write(&target, b"the bytes that get signeD").unwrap();
        assert!(
            pk.verify(std::fs::read(&target).unwrap(), &sig).is_err(),
            "a signature must not survive a change to the signed bytes"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn malformed_key_material_errors_rather_than_panics() {
        let d = tmpdir("malformed");
        let sk = d.join("short.key");
        std::fs::write(&sk, b"not a key").unwrap();

        let err = public_key_of(&sk).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("expected 64 raw bytes"));

        let _ = std::fs::remove_dir_all(&d);
    }
}
