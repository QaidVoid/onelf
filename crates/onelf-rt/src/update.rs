//! Self-update via zsync.
//!
//! Reads the update URL from `.onelf/update-url` metadata (written at pack
//! time) and uses zsync-rs to perform a delta download seeded by the current
//! executable. The new binary is written to a sibling temp file and moved
//! atomically into place.

use std::io;
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
pub fn parse_flag(args: &[String]) -> Option<UpdateFlag> {
    if args.iter().any(|a| a == "--onelf-update") {
        Some(UpdateFlag::Apply)
    } else if args.iter().any(|a| a == "--onelf-check-update") {
        Some(UpdateFlag::Check)
    } else {
        None
    }
}

/// Run the chosen update action against `self_path`, using `update_url`
/// from the package metadata. Returns a process exit code.
pub fn run(flag: UpdateFlag, self_path: &Path, update_url: &str, pubkey: &[u8]) -> i32 {
    // Refuse plaintext transports before any request is made: a MITM on
    // http:// could otherwise feed the assembly path attacker bytes.
    if !update_url.starts_with("https://") {
        eprintln!("onelf-rt: refusing non-HTTPS update URL");
        return 2;
    }
    match flag {
        UpdateFlag::Check => check(self_path, update_url),
        UpdateFlag::Apply => apply(self_path, update_url, pubkey),
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
fn verify_update_signature(path: &Path, url: &str, pubkey: &[u8]) -> Result<(), String> {
    let sig_url = detached_sig_url(url);
    // A hardened agent for the signature fetch: HTTPS only (no downgrade),
    // no redirects (a redirect is a hard error, not a silent 3xx body), and
    // a bounded timeout so a stalled server cannot hang the update.
    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(true)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent();
    let sig = agent
        .get(&sig_url)
        .call()
        .map_err(|e| format!("fetch signature: {e}"))?
        .body_mut()
        .read_to_vec()
        .map_err(|e| format!("read signature: {e}"))?;
    let data = std::fs::read(path).map_err(|e| format!("read assembled binary: {e}"))?;
    if verify_detached(pubkey, &data, &sig) {
        Ok(())
    } else {
        Err("signature does not verify against the embedded key".to_string())
    }
}

/// Build the detached-signature URL by appending `.sig` to the path
/// component, before any query string or fragment. `https://h/a?t=1`
/// becomes `https://h/a.sig?t=1`, preserving query-bearing update URLs.
fn detached_sig_url(url: &str) -> String {
    let split = url.find(['?', '#']).unwrap_or(url.len());
    let (path, rest) = url.split_at(split);
    format!("{path}.sig{rest}")
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

fn apply(self_path: &Path, url: &str, pubkey: &[u8]) -> i32 {
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

    let tmp_path = self_path.with_extension("onelf-update.tmp");
    eprintln!("onelf-rt: downloading update to {}", tmp_path.display());

    let mut assembly = match ZsyncAssembly::from_url(url, &tmp_path) {
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

    // Preserve executable bit.
    let mode = std::fs::metadata(self_path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o755);
    if let Err(e) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode)) {
        eprintln!("onelf-rt: chmod failed: {e}");
    }

    // Atomic replace.
    if let Err(e) = std::fs::rename(&tmp_path, self_path) {
        eprintln!("onelf-rt: rename failed: {e}");
        let _ = std::fs::remove_file(&tmp_path);
        return 2;
    }

    println!("updated: {current_sha} -> {remote_sha}");
    0
}

fn fetch_control(url: &str) -> Result<zsync_rs::ControlFile, String> {
    let client = zsync_rs::HttpClient::new();
    client.fetch_control_file(url).map_err(|e| format!("{e}"))
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
    fn non_https_url_is_refused_before_any_request() {
        // A plaintext URL is rejected up front (exit 2) with no network.
        let path = std::path::Path::new("/proc/self/exe");
        assert_eq!(
            super::run(super::UpdateFlag::Check, path, "http://x/app", &[]),
            2
        );
        assert_eq!(
            super::run(super::UpdateFlag::Apply, path, "ftp://x/app", &[]),
            2
        );
    }
}
