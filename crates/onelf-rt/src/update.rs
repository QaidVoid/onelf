//! Self-update via zsync.
//!
//! Reads the update URL from `.onelf/update-url` metadata (written at pack
//! time) and uses zsync-rs to perform a delta download seeded by the current
//! executable. The new binary is written to a sibling temp file and moved
//! atomically into place.

use std::ffi::OsString;
use std::io;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use zsync_rs::ZsyncAssembly;

pub enum UpdateFlag {
    /// Just report whether an update is available.
    Check,
    /// Download and apply the update.
    Apply,
}

/// Inspect argv for update flags. Returns the chosen action if matched.
pub fn parse_flag(args: &[OsString]) -> Option<UpdateFlag> {
    if args.iter().any(|a| a.as_os_str() == "--onelf-update") {
        Some(UpdateFlag::Apply)
    } else if args.iter().any(|a| a.as_os_str() == "--onelf-check-update") {
        Some(UpdateFlag::Check)
    } else {
        None
    }
}

/// Run the chosen update action against `self_path`, using `update_url`
/// from the package metadata. Returns a process exit code.
pub fn run(
    flag: UpdateFlag,
    self_path: &Path,
    update_url: &str,
    pubkey: &[u8],
    current_revision: Option<u64>,
) -> i32 {
    // Refuse plaintext transports before any request is made: a MITM on
    // http:// could otherwise feed the assembly path attacker bytes.
    if !update_url.starts_with("https://") {
        eprintln!("onelf-rt: refusing non-HTTPS update URL");
        return 2;
    }
    match flag {
        UpdateFlag::Check => check(self_path, update_url),
        UpdateFlag::Apply => apply(self_path, update_url, pubkey, current_revision),
    }
}

/// Verify a detached Ed25519 `signature` over `message` against the raw
/// 32-byte `pubkey`. Any malformed input or mismatch returns false.
fn verify_detached(pubkey: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let Ok(pk) = ed25519_compact::PublicKey::from_slice(pubkey) else {
        return false;
    };
    let Ok(sig) = ed25519_compact::Signature::from_slice(signature) else {
        return false;
    };
    pk.verify(message, &sig).is_ok()
}

/// Fetch the detached signature (`<url>.sig`) and verify it over the
/// assembled binary at `path` against the embedded public key. Fails
/// closed: any fetch/parse/verify problem is an error so the binary is
/// never installed unverified.
/// Keys a package trusts, from its embedded `.onelf/update-key` bytes: a
/// concatenation of 32-byte Ed25519 public keys. One key is the classic
/// encoding; several form a keyring, so a signature made with any key the
/// publisher has rotated in still verifies. A trailing partial key (only
/// possible in a hand-crafted package, since the packer validates the
/// length) is ignored rather than trusted.
fn keyring(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.chunks(32).filter(|k| k.len() == 32)
}

fn verify_update_signature(path: &Path, url: &str, keyring_bytes: &[u8]) -> Result<(), String> {
    let sig_url = onelf_format::update::detached_sig_url(url);
    // No redirects for the signature: a 3xx here is a hard error rather
    // than a silently substituted body.
    let agent = hardened_agent(0);
    let sig = agent
        .get(&sig_url)
        .call()
        .map_err(|e| format!("fetch signature: {e}"))?
        .body_mut()
        .read_to_vec()
        .map_err(|e| format!("read signature: {e}"))?;
    // Map the assembled binary read-only instead of reading it onto the
    // heap: it can be hundreds of MB and the kernel already has the pages
    // cached from the assembly write. The mapping stays alive through the
    // verify call and is unmapped on drop.
    let map = Mmap::open(path).map_err(|e| format!("map assembled binary: {e}"))?;
    let keys: Vec<&[u8]> = keyring(keyring_bytes).collect();
    if keys
        .iter()
        .any(|k| verify_detached(k, map.as_slice(), &sig))
    {
        Ok(())
    } else if keys.len() == 1 {
        Err("signature does not verify against the embedded key".to_string())
    } else {
        Err(format!(
            "signature does not verify against any of the {} embedded keys",
            keys.len()
        ))
    }
}

/// A read-only memory map, unmapped on drop. Used to verify a large
/// assembled update binary without copying it onto the heap.
struct Mmap {
    ptr: *mut core::ffi::c_void,
    len: usize,
}

impl Mmap {
    fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Ok(Mmap {
                ptr: core::ptr::null_mut(),
                len: 0,
            });
        }
        // SAFETY: a fresh mapping of `len` bytes from a valid fd at offset 0.
        let ptr = unsafe {
            rustix::mm::mmap(
                core::ptr::null_mut(),
                len,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::PRIVATE,
                &file,
                0,
            )?
        };
        Ok(Mmap { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `ptr`/`len` describe a live read-only mapping owned by self.
        unsafe { core::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: unmapping the region this Mmap created and still owns.
            unsafe {
                let _ = rustix::mm::munmap(self.ptr, self.len);
            }
        }
    }
}

fn check(self_path: &Path, url: &str) -> i32 {
    match fetch_control(url) {
        Ok(ctl) => {
            let current_sha = sha1_file_hex(self_path).unwrap_or_default();
            let remote_sha = ctl.sha1.clone().unwrap_or_default();
            if !remote_sha.is_empty() && current_sha == remote_sha {
                println!("up to date ({current_sha})");
                0
            } else {
                println!("update available: {current_sha} -> {remote_sha}");
                1
            }
        }
        Err(e) => {
            eprintln!("onelf-rt: check update: {e}");
            2
        }
    }
}

/// An unpredictable staging path beside the binary being updated.
///
/// A fixed, predictable name invites two attacks in a directory other
/// local users can write: two concurrent updates interleave their
/// blocks into one file, and a symlink pre-placed at the known name
/// redirects the write. The pid plus fresh random bytes make the name
/// unguessable, and [`claim_staging`] refuses anything already sitting
/// there — the signature check gates the content, never the
/// destination, so the destination must be ours alone.
fn staged_path(self_path: &Path) -> io::Result<PathBuf> {
    let mut random = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let mut hex = String::with_capacity(random.len() * 2);
    for b in random {
        hex.push_str(&format!("{b:02x}"));
    }
    let mut name = self_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".onelf-update.{}.{}.tmp", std::process::id(), hex));
    Ok(self_path.with_file_name(name))
}

/// Claim the staging path: create the file exclusively, or fail. A path
/// that already exists — a crashed run's leftover, or a planted
/// symlink — is refused rather than written through.
fn claim_staging(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Force a file's, or a directory's, contents to the disk.
fn fsync_path(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// Path of `self_path` with a suffix appended to its file name, e.g.
/// `app.onelf` to `app.onelf-prev`.
fn sibling_path(self_path: &Path, suffix: &str) -> PathBuf {
    let mut name = self_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    self_path.with_file_name(name)
}

/// The `.onelf/update-revision` entry of a packaged binary, if any.
fn staged_revision(path: &Path) -> Option<u64> {
    let mut pkg = crate::loader::load_from(path).ok()?;
    let bytes = crate::read_package_file(&mut pkg, ".onelf/update-revision")?;
    std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Pure freshness decision: a signed package may only replace a running
/// binary of strictly lower revision. When either side carries no
/// revision, fall back to the SHA1 freshness check done by the caller.
fn revision_permits(current: Option<u64>, downloaded: Option<u64>) -> Result<(), String> {
    let (current, downloaded) = match (current, downloaded) {
        (Some(c), Some(d)) => (c, d),
        _ => return Ok(()),
    };
    if downloaded <= current {
        Err(format!(
            "downloaded revision {downloaded} is not newer than the running revision {current}; \
             refusing to downgrade"
        ))
    } else {
        Ok(())
    }
}

fn apply(self_path: &Path, url: &str, pubkey: &[u8], current_revision: Option<u64>) -> i32 {
    let ctl = match fetch_control(url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("onelf-rt: fetch control: {e}");
            return 2;
        }
    };

    let current_sha = sha1_file_hex(self_path).unwrap_or_default();
    let remote_sha = ctl.sha1.clone().unwrap_or_default();
    if !remote_sha.is_empty() && current_sha == remote_sha {
        println!("already up to date ({current_sha})");
        return 0;
    }

    let tmp_path = match staged_path(self_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("onelf-rt: staging path: {e}");
            return 2;
        }
    };
    // Exclusive create: ours alone, whatever the directory holds.
    if let Err(e) = claim_staging(&tmp_path) {
        eprintln!(
            "onelf-rt: cannot claim staging file {}: {e}",
            tmp_path.display()
        );
        return 2;
    }
    eprintln!("onelf-rt: downloading update to {}", tmp_path.display());

    let mut assembly =
        match ZsyncAssembly::from_url_with_client(url, &tmp_path, update_http_client()) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("onelf-rt: init assembly: {e}");
                return 2;
            }
        };

    // Seed from our own file: most blocks usually match.
    if let Err(e) = assembly.submit_source_file(self_path) {
        eprintln!("onelf-rt: warning: seeding failed: {e}");
    }

    while !assembly.is_complete() {
        match assembly.download_missing_blocks() {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("onelf-rt: download error: {e}");
                let _ = std::fs::remove_file(&tmp_path);
                return 2;
            }
        }
    }

    if let Err(e) = assembly.complete() {
        eprintln!("onelf-rt: verify failed: {e}");
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    }

    // Verify a detached Ed25519 signature over the fully-assembled binary
    // against the pack-time public key before it is ever installed. Fails
    // closed: on any problem the temp file is removed and the running
    // executable is left unchanged.
    if let Err(e) = verify_update_signature(&tmp_path, url, pubkey) {
        eprintln!("onelf-rt: update signature check failed: {e}");
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    }

    // The revision, when both packages carry one, is the freshness gate:
    // it lives inside the package, so the signature just checked covers
    // it, and a rolled-back build cannot talk a client into installing
    // itself again. Packages without one keep the SHA1 freshness check.
    let downloaded_revision = staged_revision(&tmp_path);
    if let Err(why) = revision_permits(current_revision, downloaded_revision) {
        eprintln!("onelf-rt: refusing update: {why}");
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    }

    // Preserve executable bit.
    let mode = std::fs::metadata(self_path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o755);
    if let Err(e) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode)) {
        eprintln!("onelf-rt: chmod failed: {e}");
    }

    // A rollback point: the bytes this process was, kept beside the
    // install and forced to the disk. A failed install leaves it in
    // place for recovery.
    let prev_path = sibling_path(self_path, ".onelf-prev");
    if let Err(e) = std::fs::copy(self_path, &prev_path).and_then(|_| fsync_path(&prev_path)) {
        eprintln!(
            "onelf-rt: warning: cannot save rollback copy {}: {e}",
            prev_path.display()
        );
    }

    // The staged bytes reach the disk before the rename puts them under
    // the final name, so a power cut cannot leave a truncated binary
    // behind it; the rename itself is made durable right after.
    if let Err(e) = fsync_path(&tmp_path) {
        eprintln!("onelf-rt: warning: fsync of staged update failed: {e}");
    }

    // Atomic replace.
    if let Err(e) = std::fs::rename(&tmp_path, self_path) {
        eprintln!("onelf-rt: rename failed: {e}");
        // The rename is atomic, so the old binary is normally intact;
        // should it be gone regardless, the rollback copy goes back.
        if !self_path.exists() && std::fs::rename(&prev_path, self_path).is_ok() {
            eprintln!(
                "onelf-rt: restored the previous binary from {}",
                prev_path.display()
            );
        }
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    }
    if let Some(parent) = self_path.parent()
        && let Err(e) = fsync_path(parent)
    {
        eprintln!("onelf-rt: warning: fsync of install dir failed: {e}");
    }

    println!("updated: {current_sha} -> {remote_sha}");
    0
}

/// The transport policy for every request the update makes.
///
/// HTTPS only, so no redirect can downgrade the transfer to cleartext, and
/// a bounded timeout so a stalled server cannot hang the update. Trust
/// comes from the platform's own store rather than a set of roots compiled
/// into this binary, which is what lets a host that terminates TLS at a
/// corporate proxy update at all, and what makes the update path testable
/// against a locally issued certificate.
///
/// `redirects` is the allowance for this request. The download follows
/// them, since publishing behind a redirecting CDN is ordinary, and HTTPS
/// only plus the signature check make that safe. The signature fetch
/// allows none.
fn hardened_agent(redirects: u32) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(redirects)
        .max_redirects_will_error(true)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

/// Download `url` to `into` through the hardened client. Used for a
/// pinned GL build, which its hash verifies afterwards.
pub fn download(url: &str, into: &Path) -> Result<(), String> {
    let response = hardened_agent(10)
        .get(url)
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(into).map_err(|e| format!("{}: {e}", into.display()))?;
    io::copy(&mut reader, &mut file).map_err(|e| format!("{url}: {e}"))?;
    Ok(())
}

/// A zsync client carrying the same policy as the rest of the update.
fn update_http_client() -> zsync_rs::HttpClient {
    zsync_rs::HttpClient::with_agent(hardened_agent(10))
}

fn fetch_control(url: &str) -> Result<zsync_rs::ControlFile, String> {
    update_http_client()
        .fetch_control_file(url)
        .map_err(|e| format!("{e}"))
}

fn sha1_file_hex(path: &Path) -> io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let digest = zsync_rs::checksum::calc_sha1_stream(&mut f)?;
    let mut s = String::with_capacity(40);
    for b in digest.iter() {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

/// Resolve this binary's path on disk via `/proc/self/exe`.
pub fn self_path() -> Option<PathBuf> {
    std::fs::read_link("/proc/self/exe").ok()
}

#[cfg(test)]
mod staging_tests {
    use super::{claim_staging, fsync_path, staged_path};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onelf-stage-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn staging_names_are_unique_so_concurrent_updates_do_not_collide() {
        let dir = scratch("unique");
        let self_path = dir.join("app");
        std::fs::write(&self_path, b"current").unwrap();

        let a = staged_path(&self_path).unwrap();
        let b = staged_path(&self_path).unwrap();
        assert_ne!(a, b, "two updates must never share a staging file");

        // Both can be claimed and written at once without corrupting
        // each other; the running binary is untouched throughout.
        drop(claim_staging(&a).unwrap());
        drop(claim_staging(&b).unwrap());
        std::fs::write(&a, b"update one").unwrap();
        std::fs::write(&b, b"update two").unwrap();
        assert_eq!(std::fs::read(&self_path).unwrap(), b"current");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pre_placed_symlink_at_the_staging_path_is_refused() {
        let dir = scratch("symlink");
        let target = dir.join("victim.txt");
        std::fs::write(&target, b"do not touch").unwrap();
        let link = dir.join("app.onelf-update.pre.tmp");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Whether the attacker guessed the name or a crashed run left a
        // file there, an existing path is never written through.
        assert!(claim_staging(&link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"do not touch");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_staged_file_without_a_rename_leaves_the_binary_alone() {
        // The kill -9 case: bytes reach the staging file, the rename
        // never happens, and the installed binary is exactly as before.
        let dir = scratch("interrupt");
        let self_path = dir.join("app");
        std::fs::write(&self_path, b"the real binary").unwrap();

        let tmp = staged_path(&self_path).unwrap();
        drop(claim_staging(&tmp).unwrap());
        std::fs::write(&tmp, b"half written update").unwrap();
        fsync_path(&tmp).unwrap();
        drop(fsync_path(&dir));

        assert_eq!(std::fs::read(&self_path).unwrap(), b"the real binary");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::verify_detached;
    use ed25519_compact::{KeyPair, Seed};

    #[test]
    fn detached_signature_verifies_and_rejects_tampering() {
        let kp = KeyPair::from_seed(Seed::new([7u8; 32]));
        let pk = kp.pk.as_ref();
        let msg = b"assembled update binary bytes";
        let sig = kp.sk.sign(msg, None);

        // A good signature over the exact bytes verifies.
        assert!(verify_detached(pk, msg, sig.as_ref()));
        // A single tampered byte fails.
        assert!(!verify_detached(
            pk,
            b"assembled update binary byteX",
            sig.as_ref()
        ));
        // A different key fails.
        let other = KeyPair::from_seed(Seed::new([9u8; 32]));
        assert!(!verify_detached(other.pk.as_ref(), msg, sig.as_ref()));
        // Malformed inputs fail rather than panic.
        assert!(!verify_detached(&[0u8; 5], msg, sig.as_ref()));
        assert!(!verify_detached(pk, msg, &[0u8; 5]));
    }

    #[test]
    fn mmap_maps_file_contents_and_handles_empty() {
        use super::Mmap;
        let dir = std::env::temp_dir().join(format!("onelf-mmap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let f = dir.join("blob");
        let bytes: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        std::fs::write(&f, &bytes).unwrap();
        let map = Mmap::open(&f).unwrap();
        assert_eq!(map.as_slice(), &bytes[..]);
        drop(map);

        // Empty files map to an empty slice without touching mmap.
        let empty = dir.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert!(Mmap::open(&empty).unwrap().as_slice().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_https_url_is_refused_before_any_request() {
        // A plaintext URL is rejected up front (exit 2) with no network.
        let path = std::path::Path::new("/proc/self/exe");
        assert_eq!(
            super::run(super::UpdateFlag::Check, path, "http://x/app", &[], None),
            2
        );
        assert_eq!(
            super::run(super::UpdateFlag::Apply, path, "ftp://x/app", &[], None),
            2
        );
    }

    #[test]
    fn revision_permits_only_strictly_newer_packages() {
        use super::revision_permits;

        // Equal or lower revisions are downgrades: refused.
        assert!(revision_permits(Some(7), Some(7)).is_err());
        assert!(revision_permits(Some(7), Some(6)).is_err());
        // Strictly newer is allowed.
        assert!(revision_permits(Some(7), Some(8)).is_ok());
        // A revision on either side missing falls back to the SHA1
        // freshness check, i.e. no extra veto here.
        assert!(revision_permits(None, Some(3)).is_ok());
        assert!(revision_permits(Some(3), None).is_ok());
        assert!(revision_permits(None, None).is_ok());
    }

    #[test]
    fn keyring_splits_embedded_keys_and_ignores_a_trailing_partial() {
        use super::keyring;

        let one = [7u8; 32];
        assert_eq!(keyring(&one).count(), 1);
        assert_eq!(keyring(&one).next(), Some(&one[..]));

        let mut three = Vec::new();
        three.extend_from_slice(&[1u8; 32]);
        three.extend_from_slice(&[2u8; 32]);
        three.extend_from_slice(&[3u8; 32]);
        let keys: Vec<&[u8]> = keyring(&three).collect();
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0], &[1u8; 32]);
        assert_eq!(keys[2], &[3u8; 32]);

        // A hand-crafted 40-byte blob yields two usable keys, not a
        // mangled one; the packer refuses this length outright.
        let mut partial = three.clone();
        partial.extend_from_slice(&[9u8; 8]);
        assert_eq!(keyring(&partial).count(), 3);
    }
}
