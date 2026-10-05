//! Integrity check for packed onelf binaries.
//!
//! First the container structure: for format version 2 files, a BLAKE3
//! over the runtime ELF prefix and over the footer, both rooted in the
//! manifest. Format version 1 files carry no such hashes, so they cannot
//! be authenticated and are refused. Then a BLAKE3 of every file entry's
//! content against the hash baked into the manifest, plus the manifest
//! XXH32 checksum (already verified by the manifest reader). Returns
//! non-zero exit code when anything fails.

use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::Path;

use onelf_format::EntryKind;

use crate::extract::decompress_entry;
use crate::info::read_footer_and_manifest;

pub fn verify(binary: &Path, key: Option<&Path>, sig: Option<&Path>) -> io::Result<()> {
    // The detached signature is checked first: it covers the whole file,
    // so a failure makes the per-entry report moot, and a caller asking
    // for it wants the answer before the full sweep.
    if let (Some(key), Some(sig)) = (key, sig) {
        verify_detached_over(binary, key, sig)?;
    } else if key.is_some() != sig.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--key and --sig go together: checking a detached signature needs both",
        ));
    }

    let (footer, manifest) = read_footer_and_manifest(binary)?;
    let mut file = File::open(binary)?;

    // Format 1 has no structure hashes: the runtime prefix and the
    // footer are covered by nothing, so "verified" would be a lie.
    if footer.format_version < onelf_format::footer::FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "format version {} predates structural authentication: the runtime \
                 prefix and footer are covered by no hash, so this package cannot \
                 be verified; repack it with a current onelf",
                footer.format_version
            ),
        ));
    }

    // The manifest pins the two pieces nothing else covered: the
    // executable body in front of it, and the footer that locates
    // everything. Their hashes reaching this point means the manifest
    // bytes are already checksummed.
    let mut prefix = vec![0u8; footer.manifest_offset as usize];
    file.read_exact(&mut prefix)?;
    let prefix_hash: [u8; 32] = blake3::hash(&prefix).into();
    if prefix_hash != manifest.header.prefix_hash {
        return Err(io::Error::other(format!(
            "runtime prefix hash mismatch: expected {}, got {}; the executable \
             body was modified",
            hex32(&manifest.header.prefix_hash),
            hex32(&prefix_hash)
        )));
    }

    let mut footer_buf = [0u8; onelf_format::FOOTER_SIZE];
    file.seek(std::io::SeekFrom::End(-(onelf_format::FOOTER_SIZE as i64)))?;
    file.read_exact(&mut footer_buf)?;
    let footer_from_file = onelf_format::Footer::from_bytes(&footer_buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad footer: {e}")))?;
    let footer_hash = footer_from_file.structure_hash();
    if footer_hash != manifest.header.footer_hash {
        return Err(io::Error::other(format!(
            "footer hash mismatch: expected {}, got {}; the footer was modified",
            hex32(&manifest.header.footer_hash),
            hex32(&footer_hash)
        )));
    }
    println!(
        "Structure: OK (format {}, prefix and footer authenticated)",
        footer.format_version
    );

    let dict = crate::info::read_dict(&mut file, &footer)?;

    let file_entries: Vec<(usize, String)> = manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EntryKind::File)
        .map(|(i, _)| (i, manifest.entry_path(i)))
        .collect();

    let total = file_entries.len();
    let mut failed: Vec<(String, String)> = Vec::new();

    for (idx, rel_path) in &file_entries {
        let entry = &manifest.entries[*idx];
        let data = match decompress_entry(&mut file, &footer, entry, dict.as_deref()) {
            Ok(data) => data,
            // A block that does not decode or does not hash is this file's
            // failure, reported with the rest rather than ending the run.
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                failed.push((rel_path.clone(), e.to_string()));
                continue;
            }
            Err(e) => return Err(e),
        };
        let actual: [u8; 32] = blake3::hash(&data).into();
        if actual != entry.content_hash {
            failed.push((
                rel_path.clone(),
                format!(
                    "expected {}, got {}",
                    hex32(&entry.content_hash),
                    hex32(&actual)
                ),
            ));
        }
    }

    println!("Checked {total} file(s)");
    if failed.is_empty() {
        println!("OK");
        Ok(())
    } else {
        eprintln!("{} file(s) failed verification:", failed.len());
        for (path, msg) in &failed {
            eprintln!("  {path}: {msg}");
        }
        Err(io::Error::other("verification failed"))
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Check a detached Ed25519 signature over the whole binary, mirroring
/// what the runtime's self-update does before installing. Both inputs
/// are the raw fixed-width encodings `key new` and `sign` produce: a
/// 32-byte public key, a 64-byte signature.
fn verify_detached_over(binary: &Path, key: &Path, sig: &Path) -> io::Result<()> {
    use ed25519_compact::{PublicKey, Signature};

    let key_bytes = std::fs::read(key)?;
    let pk = PublicKey::from_slice(&key_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not a public key: expected {} raw bytes, found {}",
                key.display(),
                PublicKey::BYTES,
                key_bytes.len()
            ),
        )
    })?;
    let sig_bytes = std::fs::read(sig)?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not a signature: expected {} raw bytes, found {}",
                sig.display(),
                Signature::BYTES,
                sig_bytes.len()
            ),
        )
    })?;
    let message = std::fs::read(binary)?;
    if pk.verify(&message, &signature).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the signature over {} does not verify: the file changed after signing, \
                 or the key is not the signer's",
                binary.display()
            ),
        ));
    }
    println!("Signature: OK ({})", key.display());
    Ok(())
}
