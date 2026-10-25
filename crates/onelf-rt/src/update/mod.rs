//! Self-update from the release binary itself.
//!
//! Reads the update URL from `.onelf/update-url` metadata (written at pack
//! time). The URL names the release binary: the runtime fetches its footer
//! and block manifest with two small range requests, downloads only the
//! content it does not already have, and copies the rest from the running
//! executable. The new binary lands in a sibling temp file and is moved
//! atomically into place after a detached Ed25519 signature over the
//! assembled bytes verifies. No control file is published or fetched.
//!
//! A package packed with the unsigned opt-in (`.onelf/update-unsigned`,
//! see [`policy`]) carries no signing key and verifies nothing: every
//! update step announces on stderr that the release server is trusted in
//! full. Signing stays the default and a keyed package never skips
//! verification, whichever markers it carries.
//!
//! `range` carries the HTTPS transport and the remote probe; `delta`
//! plans and writes the assembled file.

mod delta;
mod range;

use std::ffi::OsString;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use range::{FetchError, HttpsRangeSource, RangeSource, RemotePackage};

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

/// Resolve the update policy a package carries into the verification
/// behavior: `Some(keys)` verifies signatures against that keyring,
/// `None` updates without verification. A package with both a keyring
/// and the unsigned marker is refused rather than guessed about: the
/// packer never produces one, so it is hand-crafted or tampered with.
/// A URL without either means self-update stays disabled, as before.
pub fn policy(key: Option<Vec<u8>>, unsigned: bool) -> Result<Option<Vec<u8>>, String> {
    match (key, unsigned) {
        (Some(keys), false) => Ok(Some(keys)),
        (Some(_), true) => Err(
            "package has both a signing key and the unsigned marker; refusing to \
             guess which policy it means"
                .to_string(),
        ),
        (None, true) => Ok(None),
        (None, false) => Err(
            "self-update disabled: package has an update URL but no signing \
             key (repack with --update-key, or --update-unsigned to accept \
             unsigned updates)"
                .to_string(),
        ),
    }
}

/// Run the chosen update action against `self_path`, using `update_url`
/// from the package metadata. `pubkey` is the package's policy: `None`
/// updates without signature verification, warning on stderr every time.
/// Returns a process exit code.
pub fn run(
    flag: UpdateFlag,
    self_path: &Path,
    update_url: &str,
    pubkey: Option<&[u8]>,
    current_revision: Option<u64>,
) -> i32 {
    // Refuse plaintext transports before any request is made: a MITM on
    // http:// could otherwise feed the assembly path attacker bytes.
    if !update_url.starts_with("https://") {
        eprintln!("onelf-rt: refusing non-HTTPS update URL");
        return 2;
    }
    if pubkey.is_none() {
        eprintln!(
            "onelf-rt: warning: this package verifies no update signatures; \
             every update installs whatever the release server serves"
        );
    }
    match flag {
        UpdateFlag::Check => check(self_path, update_url, pubkey, current_revision),
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

/// A signature is 64 bytes; anything the server streams beyond this is
/// not one, and must not be buffered.
const SIG_MAX_BYTES: u64 = 64 * 1024;

fn verify_update_signature(path: &Path, url: &str, keyring_bytes: &[u8]) -> Result<(), String> {
    let sig_url = onelf_format::update::detached_sig_url(url);
    // Redirects are followed as for the package itself, over HTTPS only:
    // release hosts such as GitHub answer every asset with a redirect to
    // their CDN. A substituted body cannot pass the Ed25519 check below,
    // which is what guards the update.
    let agent = hardened_agent(10);
    let mut sig = Vec::new();
    agent
        .get(&sig_url)
        .call()
        .map_err(|e| format!("fetch signature: {e}"))?
        .into_body()
        .into_reader()
        .take(SIG_MAX_BYTES)
        .read_to_end(&mut sig)
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

/// An exclusive advisory lock on the binary being updated, held for the
/// lifetime of the value. `flock(2)` sits on the inode, so it survives a
/// concurrent process renaming over the name and disappears with this
/// process.
struct InstallLock {
    _file: std::fs::File,
}

impl InstallLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
        Ok(InstallLock { _file: file })
    }
}

fn check(self_path: &Path, url: &str, pubkey: Option<&[u8]>, current_revision: Option<u64>) -> i32 {
    let local_id = local_package_id(self_path);
    let src = HttpsRangeSource::new(url.to_string());
    let remote = match range::probe(&src) {
        Ok(remote) => remote,
        Err(FetchError::NoRanges(why)) => {
            // The host ignores Range, so freshness needs the whole file.
            // A rare case: ordinary static file servers answer ranges.
            eprintln!("onelf-rt: {why}; checking against a full download");
            return match fetch_verified(self_path, url, pubkey, None, &src) {
                Err(code) => code,
                Ok(staged) => {
                    let (revision, id) = staged_info(&staged);
                    let _ = std::fs::remove_file(&staged);
                    if id.is_some() && id == local_id {
                        println!("up to date");
                        return 0;
                    }
                    report_available(local_id.as_ref(), id.as_ref(), current_revision, revision)
                }
            };
        }
        Err(FetchError::Other(e)) => {
            eprintln!("onelf-rt: check update: {e}");
            return 2;
        }
    };

    // The package id covers every content hash and the runtime prefix, so
    // equal ids mean equal packages: a two-request freshness check.
    if Some(remote.package_id()) == local_id.as_ref() {
        println!("up to date ({})", hex(remote.package_id()));
        return 0;
    }
    if current_revision.is_none() {
        println!(
            "update available: {} -> {}",
            id_or_dash(local_id.as_ref()),
            hex(remote.package_id())
        );
        return 1;
    }

    // Only the remote package's own bytes carry its revision, so a running
    // package with one has the update fetched and verified as an apply
    // would, and an update the apply would refuse is not offered.
    match fetch_verified(self_path, url, pubkey, Some(remote), &src) {
        Err(code) => code,
        Ok(staged) => {
            let (revision, id) = staged_info(&staged);
            let _ = std::fs::remove_file(&staged);
            report_available(local_id.as_ref(), id.as_ref(), current_revision, revision)
        }
    }
}

/// The verdict after the signed bytes settled freshness and revision.
fn report_available(
    local_id: Option<&[u8; 32]>,
    staged_id: Option<&[u8; 32]>,
    current_revision: Option<u64>,
    staged_revision: Option<u64>,
) -> i32 {
    match revision_permits(current_revision, staged_revision) {
        Err(why) => {
            println!("no update offered: {why}");
            0
        }
        Ok(()) => {
            println!(
                "update available: {} -> {}",
                id_or_dash(local_id),
                id_or_dash(staged_id)
            );
            1
        }
    }
}

/// The running package's id, when this binary is a readable package.
fn local_package_id(self_path: &Path) -> Option<[u8; 32]> {
    crate::loader::load_from(self_path)
        .ok()
        .map(|p| p.manifest.header.package_id)
}

/// A package id as hex, or a dash when there is none to show.
fn id_or_dash(id: Option<&[u8; 32]>) -> String {
    id.map(|b| hex(b.as_slice())).unwrap_or_else(|| "-".into())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
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

/// Copy `self_path` to `prev_path` as the rollback point.
///
/// The name is predictable, so in a directory other users can write
/// something may already sit there, a symlink to a file of ours among
/// them. The copy is made in a staging file claimed the way an update's
/// is and renamed over the name, which replaces whatever was there
/// rather than writing through it.
fn save_rollback(self_path: &Path, prev_path: &Path) -> io::Result<()> {
    let tmp = staged_path(self_path)?;
    let copied = (|| {
        let mut out = claim_staging(&tmp)?;
        io::copy(&mut std::fs::File::open(self_path)?, &mut out)?;
        let mode = std::fs::metadata(self_path)?.permissions().mode();
        out.set_permissions(std::fs::Permissions::from_mode(mode))?;
        out.sync_all()?;
        std::fs::rename(&tmp, prev_path)
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    copied
}

/// The `.onelf/update-revision` entry and package id of a packaged
/// binary, read from its own bytes.
fn staged_info(path: &Path) -> (Option<u64>, Option<[u8; 32]>) {
    let mut pkg = match crate::loader::load_from(path) {
        Ok(p) => p,
        Err(_) => return (None, None),
    };
    let id = pkg.manifest.header.package_id;
    let revision = crate::read_package_file(&mut pkg, ".onelf/update-revision")
        .and_then(|bytes| {
            std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
        });
    (revision, Some(id))
}

/// Pure freshness decision: a signed package may only replace a running
/// binary of strictly lower revision. A running binary without a revision
/// falls back to the package-id comparison done by the caller. One that
/// carries a revision refuses a package without one, since every release
/// signed before revisions existed would otherwise roll it back.
fn revision_permits(current: Option<u64>, downloaded: Option<u64>) -> Result<(), String> {
    let (current, downloaded) = match (current, downloaded) {
        (Some(c), Some(d)) => (c, d),
        (Some(c), None) => {
            return Err(format!(
                "the downloaded package carries no revision and the running one is at \
                 revision {c}; refusing what may be an older release"
            ));
        }
        (None, _) => return Ok(()),
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

/// Download the update beside `self_path` and verify its signature,
/// returning the staged file. `remote` is the probe result when the host
/// honors range requests; without it the release downloads in full. A
/// `None` keyring (the unsigned opt-in) skips the signature step. On
/// failure nothing is left behind, and the error is the exit code, already
/// explained on stderr.
fn fetch_verified(
    self_path: &Path,
    url: &str,
    pubkey: Option<&[u8]>,
    remote: Option<RemotePackage>,
    src: &dyn RangeSource,
) -> Result<PathBuf, i32> {
    let tmp_path = match staged_path(self_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("onelf-rt: staging path: {e}");
            return Err(2);
        }
    };
    // Exclusive create: ours alone, whatever the directory holds.
    let staged = match claim_staging(&tmp_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "onelf-rt: cannot claim staging file {}: {e}",
                tmp_path.display()
            );
            return Err(2);
        }
    };
    eprintln!("onelf-rt: downloading update to {}", tmp_path.display());

    if let Some(remote) = &remote {
        match delta::assemble(self_path, remote, &staged, src) {
            Ok(stats) => {
                eprintln!(
                    "onelf-rt: reused {} blocks, fetching {} of {} bytes",
                    stats.reused, stats.fetched, remote.size
                );
                match pubkey {
                    // Copied bytes are only as good as the assumption that
                    // identical content encodes identically. When that
                    // assumption breaks the signature names it, and a full
                    // download replaces the guess with certainty.
                    Some(keys) => match verify_update_signature(&tmp_path, url, keys) {
                        Ok(()) => return Ok(tmp_path),
                        Err(e) if stats.reused > 0 => {
                            eprintln!(
                                "onelf-rt: assembled update failed its signature check ({e}); \
                                 retrying with a full download"
                            );
                        }
                        Err(e) => {
                            eprintln!("onelf-rt: update signature check failed: {e}");
                            let _ = std::fs::remove_file(&tmp_path);
                            return Err(2);
                        }
                    },
                    // Unsigned policy: the assembled bytes install as-is.
                    None => return Ok(tmp_path),
                }
            }
            Err(why) => {
                eprintln!("onelf-rt: delta update unavailable ({why}); downloading in full");
            }
        }
    }

    // Full download into the staging file already claimed above.
    let mut staged = staged;
    if let Err(e) = download_into(url, &mut staged) {
        eprintln!("onelf-rt: download update: {e}");
        let _ = std::fs::remove_file(&tmp_path);
        return Err(2);
    }
    if let Some(keys) = pubkey
        && let Err(e) = verify_update_signature(&tmp_path, url, keys)
    {
        eprintln!("onelf-rt: update signature check failed: {e}");
        let _ = std::fs::remove_file(&tmp_path);
        return Err(2);
    }
    Ok(tmp_path)
}

fn apply(self_path: &Path, url: &str, pubkey: Option<&[u8]>, current_revision: Option<u64>) -> i32 {
    // One installer at a time: two concurrent applies would both pass the
    // freshness gate and race their renames, and the older release could
    // win. The lock sits on the running binary's inode and lives until
    // this process exits; the binding is held to the end of the function.
    let _install_lock = match InstallLock::acquire(self_path) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("onelf-rt: cannot lock {} for updating: {e}", self_path.display());
            return 2;
        }
    };
    let local_id = local_package_id(self_path);
    let src = HttpsRangeSource::new(url.to_string());
    let remote = match range::probe(&src) {
        Ok(remote) => Some(remote),
        Err(FetchError::NoRanges(why)) => {
            eprintln!("onelf-rt: {why}; downloading in full");
            None
        }
        Err(FetchError::Other(e)) => {
            eprintln!("onelf-rt: fetch release: {e}");
            return 2;
        }
    };

    // The package id covers every content hash and the runtime prefix, so
    // equal ids mean equal packages: nothing to install.
    if let Some(remote) = &remote
        && Some(remote.package_id()) == local_id.as_ref()
    {
        println!("already up to date ({})", hex(remote.package_id()));
        return 0;
    }

    let tmp_path = match fetch_verified(self_path, url, pubkey, remote, &src) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // The signature verified the bytes under `tmp_path`, and the rename
    // below installs whatever sits there when it runs. Pin the file's
    // identity now, before the slower steps, and re-check it just before
    // the rename: a local attacker with write access to this directory
    // must not swap the verified file for their own while the rollback
    // copy and the fsyncs run.
    let verified_identity = match std::fs::metadata(&tmp_path) {
        Ok(m) => (m.dev(), m.ino()),
        Err(e) => {
            eprintln!("onelf-rt: staged update disappeared: {e}");
            let _ = std::fs::remove_file(&tmp_path);
            return 2;
        }
    };

    // The revision is the freshness gate once the running package carries
    // one: it lives inside the package, so the signature just checked
    // covers it under the signed policy, and a rolled-back build cannot
    // talk a client into installing itself again. The package-id
    // freshness check stands in for a running package without one.
    let (downloaded_revision, downloaded_id) = staged_info(&tmp_path);
    let Some(downloaded_id) = downloaded_id else {
        eprintln!(
            "onelf-rt: the downloaded update is not a readable onelf package; \
             refusing to install it"
        );
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    };
    if let Err(why) = revision_permits(current_revision, downloaded_revision) {
        println!("no update offered: {why}");
        let _ = std::fs::remove_file(&tmp_path);
        return 0;
    }

    // Preserve executable bit. A failure here would install a binary the
    // system cannot run, so it fails the update rather than the exec.
    let mode = std::fs::metadata(self_path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o755);
    if let Err(e) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode)) {
        eprintln!("onelf-rt: chmod failed: {e}");
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    }

    // A rollback point: the bytes this process was, kept beside the
    // install and forced to the disk. A failed install leaves it in
    // place for recovery. Without it the update would destroy the only
    // known-good bytes, so a failure fails the update; it can be
    // retried.
    let prev_path = sibling_path(self_path, ".onelf-prev");
    if let Err(e) = save_rollback(self_path, &prev_path) {
        eprintln!(
            "onelf-rt: cannot save rollback copy {}: {e}",
            prev_path.display()
        );
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    }

    // The staged bytes reach the disk before the rename puts them under
    // the final name, so a power cut cannot leave a truncated binary
    // behind it; the rename itself is made durable right after.
    if let Err(e) = fsync_path(&tmp_path) {
        eprintln!("onelf-rt: warning: fsync of staged update failed: {e}");
    }

    // Atomic replace. The identity re-check must be the last thing before
    // the rename.
    match std::fs::metadata(&tmp_path) {
        Ok(m) if (m.dev(), m.ino()) == verified_identity => {}
        _ => {
            eprintln!(
                "onelf-rt: the staged update changed after it was verified; \
                 refusing to install it"
            );
            let _ = std::fs::remove_file(&tmp_path);
            return 2;
        }
    }
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

    println!(
        "updated: {} -> {}",
        id_or_dash(local_id.as_ref()),
        id_or_dash(Some(&downloaded_id))
    );
    0
}

/// The transport policy shared by every request the update makes.
///
/// HTTPS only, so no redirect can downgrade the transfer to cleartext, and
/// bounded redirects so a server cannot chain them forever. Trust comes
/// from the platform's own store rather than a set of roots compiled into
/// this binary, which is what lets a host that terminates TLS at a
/// corporate proxy update at all, and what makes the update path testable
/// against a locally issued certificate.
///
/// `redirects` is the allowance for this request. The download follows
/// them, since publishing behind a redirecting CDN is ordinary, and HTTPS
/// only plus the signature check make that safe.
fn transport(redirects: u32) -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(redirects)
        .max_redirects_will_error(true)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
}

/// The client for control requests: the release probe and the signature
/// fetch. Their bodies are bytes to kilobytes, so a single end-to-end
/// budget keeps a stalled server from hanging the update.
fn hardened_agent(redirects: u32) -> ureq::Agent {
    transport(redirects)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent()
}

/// The client for requests that stream a body: whole-file downloads and
/// ranged block fetches. The budget is per read rather than for the whole
/// transfer, because a total would cut off a full download on a slow link,
/// while a stalled connection still fails within 30 seconds of its last
/// byte.
fn streaming_agent(redirects: u32) -> ureq::Agent {
    transport(redirects)
        .timeout_recv_body(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent()
}

/// Download `url` through the hardened client. Used for a pinned GL
/// build, which its hash verifies afterwards.
pub fn download(url: &str, into: &Path) -> Result<(), String> {
    let mut file = std::fs::File::create(into).map_err(|e| format!("{}: {e}", into.display()))?;
    download_into(url, &mut file)
}

/// Stream `url` into an open file from its start.
fn download_into(url: &str, file: &mut std::fs::File) -> Result<(), String> {
    let response = streaming_agent(10)
        .get(url)
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let mut reader = response.into_body().into_reader();
    file.set_len(0).map_err(|e| format!("{url}: {e}"))?;
    file.seek(io::SeekFrom::Start(0))
        .map_err(|e| format!("{url}: {e}"))?;
    io::copy(&mut reader, file).map_err(|e| format!("{url}: {e}"))?;
    Ok(())
}

/// Resolve this binary's path on disk via `/proc/self/exe`.
pub fn self_path() -> Option<PathBuf> {
    std::fs::read_link("/proc/self/exe").ok()
}

#[cfg(test)]
mod staging_tests {
    use super::{claim_staging, fsync_path, save_rollback, staged_path};
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
    fn a_planted_symlink_at_the_rollback_name_is_replaced_not_followed() {
        let dir = scratch("rollback");
        let self_path = dir.join("app");
        std::fs::write(&self_path, b"the running binary").unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, b"do not touch").unwrap();
        let prev = dir.join("app.onelf-prev");
        std::os::unix::fs::symlink(&victim, &prev).unwrap();

        save_rollback(&self_path, &prev).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch");
        assert!(!prev.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&prev).unwrap(), b"the running binary");

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
        // A plaintext URL is rejected up front (exit 2) with no network,
        // whichever policy the package carries.
        let path = std::path::Path::new("/proc/self/exe");
        assert_eq!(
            super::run(
                super::UpdateFlag::Check,
                path,
                "http://x/app",
                Some(&[7u8; 32]),
                None
            ),
            2
        );
        assert_eq!(
            super::run(super::UpdateFlag::Apply, path, "ftp://x/app", None, None),
            2
        );
    }

    #[test]
    fn policy_resolves_keys_markers_and_refusals() {
        use super::policy;

        let keys = vec![7u8; 32];

        // A keyed package verifies; the plain keyless one stays disabled.
        assert_eq!(policy(Some(keys.clone()), false), Ok(Some(keys.clone())));
        assert!(policy(None, false).is_err());

        // The unsigned opt-in is honored only without a key: a package
        // carrying both is refused rather than guessed about.
        assert_eq!(policy(None, true), Ok(None));
        assert!(policy(Some(keys), true).is_err());
    }

    #[test]
    fn revision_permits_only_strictly_newer_packages() {
        use super::revision_permits;

        // Equal or lower revisions are downgrades: refused.
        assert!(revision_permits(Some(7), Some(7)).is_err());
        assert!(revision_permits(Some(7), Some(6)).is_err());
        // Strictly newer is allowed.
        assert!(revision_permits(Some(7), Some(8)).is_ok());
        // A running package without a revision falls back to the SHA1
        // freshness check, i.e. no extra veto here.
        assert!(revision_permits(None, Some(3)).is_ok());
        assert!(revision_permits(None, None).is_ok());
        // Once the running package carries one, a package without one is
        // refused: it may be any release signed before revisions existed.
        assert!(revision_permits(Some(3), None).is_err());
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

        // A hand-crafted 104-byte blob yields three usable keys, not a
        // mangled fourth; the packer refuses this length outright.
        let mut partial = three.clone();
        partial.extend_from_slice(&[9u8; 8]);
        assert_eq!(keyring(&partial).count(), 3);
    }
}
