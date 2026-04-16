//! Integrity check for packed onelf binaries.
//!
//! Recomputes BLAKE3 of every file entry's content and compares it against
//! the hash baked into the manifest. Also confirms the manifest XXH32
//! checksum (already verified by the manifest reader) and reports any
//! mismatch. Returns non-zero exit code when any entry fails.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use onelf_format::EntryKind;

use crate::extract::decompress_entry;
use crate::info::read_footer_and_manifest;

pub fn verify(binary: &Path) -> io::Result<()> {
    let (footer, manifest) = read_footer_and_manifest(binary)?;
    let mut file = File::open(binary)?;

    let dict = if footer.dict_size > 0 {
        file.seek(SeekFrom::Start(footer.dict_offset))?;
        let mut buf = vec![0u8; footer.dict_size as usize];
        file.read_exact(&mut buf)?;
        Some(buf)
    } else {
        None
    };

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
        let data = decompress_entry(&mut file, &footer, entry, dict.as_deref())?;
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
