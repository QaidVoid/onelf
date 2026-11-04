//! `onelf sysroot`: obtain and inspect a pinned sysroot.

use std::io::{self, Write};
use std::path::Path;

use onelf_sysroot::{Database, archive};

use crate::bundle::elf::audit_unbundled_needs;
use crate::bundle::remove_dangling_links_beside;
use crate::pack::{HostLibs, PackOptions};

/// Materialize the rootfs archive at `source`, a local path or an
/// `https://` URL, into `dir`. With `hash`, the archive's BLAKE3 has to
/// match before anything is unpacked: a sysroot is executable code, and
/// TLS says who served it, not that it is the one the recipe was written
/// against.
pub fn fetch(source: &str, dir: &Path, hash: Option<&str>) -> io::Result<()> {
    if source.starts_with("http://") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a sysroot is executable code; fetch it over https:// only",
        ));
    }
    if let Some(hash) = hash {
        onelf_sysroot::platform::check_hash(hash)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }
    if let Some(rest) = source.strip_prefix("https://") {
        let name = rest.rsplit('/').next().unwrap_or("sysroot.tar");
        let staged = dir.with_extension(format!("download-{}", std::process::id()));
        let downloaded = download(source, &staged);
        let result = downloaded.and_then(|()| {
            check_archive_hash(&staged, hash)?;
            eprintln!("Materializing {name} into {}", dir.display());
            archive::materialize(&staged, dir)
        });
        let _ = std::fs::remove_file(&staged);
        return result;
    }
    let archive_path = Path::new(source);
    check_archive_hash(archive_path, hash)?;
    eprintln!(
        "Materializing {} into {}",
        archive_path.display(),
        dir.display()
    );
    archive::materialize(archive_path, dir)
}

fn check_archive_hash(archive: &Path, expected: Option<&str>) -> io::Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = hash_file(archive)?;
    if actual != expected.to_ascii_lowercase() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: hash mismatch (expected {expected}, got {actual}); not materializing",
                archive.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn download(url: &str, into: &Path) -> io::Result<()> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| io::Error::other(format!("{url}: {e}")))?;
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(into)?;
    io::copy(&mut reader, &mut file)?;
    file.flush()
}

/// Print what the sysroot at `dir` holds.
pub fn info(dir: &Path) -> io::Result<()> {
    let db = Database::read(dir)?;
    let packages: Vec<_> = db.packages().collect();
    let files: usize = packages.iter().map(|p| p.files.len()).sum();
    println!("sysroot: {}", dir.display());
    println!("packages: {}", packages.len());
    println!("files:    {files}");
    match db.package("glibc") {
        Some(glibc) => println!("glibc:    {}", glibc.version),
        None => println!("glibc:    not installed"),
    }
    Ok(())
}

/// The entrypoint a GL build carries so it packs like any package. The
/// runtime never runs it.
const GL_ENTRYPOINT: &str = "bin/onelf-gl";

/// The entrypoint a shared set carries, for the same reason.
const SET_ENTRYPOINT: &str = "bin/onelf-set";

/// Pack the GL tree at `dir` into `output` and print the hash to pin.
/// `sysroot` is the sysroot the tree was built from, if any.
///
/// The tree has to be self-contained apart from glibc, which the package
/// that uses it carries, and the driver families it exists to provide.
/// A library in `lib/` needing anything else the tree does not carry is
/// an error here rather than a silent failure on a host with nothing to
/// fall back on, unless the sysroot holds that need nowhere, which makes
/// the library unloadable from any build. Other objects with an unmet
/// need are dropped and listed: a closure carries bindings for stacks
/// outside it, and nothing loads those from a build.
pub fn pack_gl(
    dir: &Path,
    output: &Path,
    sysroot: Option<&Path>,
    runtime: &[u8],
) -> io::Result<()> {
    pack_tree(
        dir,
        output,
        sysroot,
        runtime,
        GL_ENTRYPOINT,
        "onelf-gl",
        None,
    )?;
    let hash = hash_file(output)?;
    println!("blake3 = \"{hash}\"");
    Ok(())
}

/// Pack the shared set tree at `dir` into `output` under `name` and print
/// the hash to pin. The same rules as a GL build: glibc stays out, and
/// a library the set needs beyond glibc and the driver families has to
/// be inside it.
///
/// The packed build carries a provider index derived from the objects it
/// verified during assembly. It is packed beside `output` and only
/// renamed onto it once the packed artifact has been read back and found
/// to carry exactly that index: the pin printed below is for a build
/// whose exported names were checked, not for one whose word was taken.
pub fn pack_set(
    dir: &Path,
    output: &Path,
    name: &str,
    sysroot: Option<&Path>,
    runtime: &[u8],
) -> io::Result<()> {
    crate::bundle::sysroot::check_set_name(name)?;
    let staged = crate::pack::sibling_tmp(output, "unverified");
    let packed = pack_tree(
        dir,
        &staged,
        sysroot,
        runtime,
        SET_ENTRYPOINT,
        &format!("onelf-set-{name}"),
        Some(name),
    );
    let hash = publish_checked(&staged, output, |staged| {
        packed?;
        let exports = crate::bundle::sysroot::verified_exports(dir)?;
        crate::bundle::sysroot::verify_set_artifact(staged, name, &exports)
    })?;
    println!("blake3 = \"{hash}\"");
    Ok(())
}

/// Rename `staged` onto `output` once `check` accepts it and return what
/// the check answered. A rejected artifact is removed instead, so the
/// output path never holds bytes the check refused and a previous output
/// there stays as it was.
fn publish_checked(
    staged: &Path,
    output: &Path,
    check: impl FnOnce(&Path) -> io::Result<String>,
) -> io::Result<String> {
    match check(staged) {
        Ok(answer) => {
            std::fs::rename(staged, output)?;
            Ok(answer)
        }
        Err(e) => {
            let _ = std::fs::remove_file(staged);
            Err(e)
        }
    }
}

/// Record the name every shared object in `dir` answers to.
///
/// Every directory of a set that holds a shared object goes on the
/// launch's library path, so a name anywhere in the tree is a name the
/// loader will find.
fn write_shipped_sonames(dir: &Path) -> io::Result<()> {
    let mut names: Vec<String> = jwalk::WalkDir::new(dir)
        .skip_hidden(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() || e.path_is_symlink())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.contains(".so"))
        .collect();
    names.sort();
    names.dedup();
    let path = dir.join(crate::bundle::sysroot::SHIPPED_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, names.join("\n") + "\n")?;
    crate::bundle::normalize_mtime(&path);
    Ok(())
}

fn pack_tree(
    dir: &Path,
    output: &Path,
    sysroot: Option<&Path>,
    runtime: &[u8],
    entrypoint: &str,
    name: &str,
    set: Option<&str>,
) -> io::Result<()> {
    let entry = dir.join(entrypoint);
    if !entry.exists() {
        if let Some(parent) = entry.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &entry,
            "#!/bin/sh\necho 'onelf dependency build; not meant to be run'\n",
        )?;
        std::fs::set_permissions(
            &entry,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )?;
    }

    if let Some(name) = std::fs::read_dir(dir.join("lib"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .find(|name| name.starts_with("libc.so") || name.starts_with("ld-linux"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "lib/{name}: a build takes glibc from the package that uses it; leave the glibc package's files out"
            ),
        ));
    }

    // The tree keeps the run paths the distribution wrote, and those name
    // the host. Rewriting them first means the audit below judges the
    // tree as it will actually be loaded.
    let (rewritten, _, _, _) = crate::bundle::elf::finalize_tree(dir, Path::new("lib"));
    if rewritten > 0 {
        eprintln!("Rewrote the run path of {rewritten} object(s)");
    }

    // Every directory in the tree that holds a shared object is on the
    // launch's library path, so a soname the tree carries anywhere is
    // one the loader will find.
    let carried: std::collections::HashSet<String> = jwalk::WalkDir::new(dir)
        .skip_hidden(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() || e.path_is_symlink())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.contains(".so"))
        .collect();

    let mut findings = audit_unbundled_needs(dir, Path::new("lib"));
    for (_, libs) in &mut findings {
        libs.retain(|s| !carried.contains(s));
        libs.retain(|s| {
            !onelf_format::drivers::DRIVER_FAMILIES
                .iter()
                .any(|p| s.starts_with(p))
                && !onelf_format::resolve::is_libc_family(s)
        });
    }
    findings.retain(|(_, libs)| !libs.is_empty());
    // A library directly in lib/ is one a package elsewhere resolves by
    // soname, a driver the build exists to provide say, so missing it is
    // the build being one package short. It is dropped only when the
    // sysroot holds its need nowhere, an optional dependency never
    // installed, since then no build could load it.
    let held: Option<std::collections::HashSet<String>> = sysroot
        .map(|root| {
            Database::read(root).map(|db| {
                db.packages()
                    .flat_map(|p| p.files.iter())
                    .filter_map(|f| f.rsplit('/').next())
                    .map(String::from)
                    .collect()
            })
        })
        .transpose()?;
    for (object, libs) in &findings {
        let in_lib = object.parent() == Some(&dir.join("lib"))
            && object
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".so"));
        if !in_lib
            || held
                .as_ref()
                .is_some_and(|h| libs.iter().any(|s| !h.contains(s)))
        {
            continue;
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} needs {}, which the build does not carry; name the package that provides it",
                object.strip_prefix(dir).unwrap_or(object).display(),
                libs[0]
            ),
        ));
    }
    // A closure carries bindings for stacks outside it, a Python module
    // of util-linux say. Nothing loads those from a build, so they go, by
    // name, rather than failing it. What keeps a package from leaving out
    // a library on the strength of a build that dropped it is the record
    // written below.
    if !findings.is_empty() {
        eprintln!(
            "Dropped {} object(s) needing libraries outside the build:",
            findings.len()
        );
        for (object, libs) in &findings {
            eprintln!(
                "  {} (needs {})",
                object.strip_prefix(dir).unwrap_or(object).display(),
                libs[0]
            );
            std::fs::remove_file(object)?;
            remove_dangling_links_beside(object);
        }
    }

    // What the build ships, for the packages that leave it out to read
    // rather than work out a second time. The two answers drifted apart
    // before: a package pinning a set left out a library the set had
    // never carried, and nothing held it at launch.
    write_shipped_sonames(dir)?;
    let provider_index = match set {
        Some(set) => Some(crate::bundle::sysroot::encode_provider_index(dir, set)?),
        None => None,
    };

    crate::pack::pack(
        &PackOptions {
            directory: dir.to_path_buf(),
            output: output.to_path_buf(),
            command: entrypoint.to_string(),
            name: Some(name.to_string()),
            entrypoints: Vec::new(),
            default_entrypoint: None,
            lib_dirs: vec!["auto".to_string()],
            level: 19,
            block_size: crate::compress::DEFAULT_BLOCK_SIZE,
            payload: crate::pack::Payload::Blocks,
            use_dict: false,
            no_compress: false,
            memfd: Some(false),
            working_dir: onelf_format::WorkingDir::Inherit,
            host_libs: HostLibs::Never,
            cache: false,
            prefer_cache: false,
            update_url: None,
            embed_updater: false,
            update_key: None,
            update_revision: None,
            update_unsigned: false,
            exclude: Vec::new(),
            package_info: None,
            mtime: Some(0),
            env: Vec::new(),
            paths: Vec::new(),
            preload: Vec::new(),
            needs_setuid: false,
            profile: Vec::new(),
            provider_index,
            metadata: crate::pack::MetadataPayload::default(),
            // A sysroot build states no capability requirements of its
            // own; the caller that embeds the runtime probes it.
            runtime_note: onelf_format::elf::RuntimeNote::Absent,
        },
        runtime,
    )?;

    Ok(())
}

/// Lowercase hex BLAKE3 of the file at `path`, streamed.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::publish_checked;

    /// A scratch directory unique to this test run.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onelf-publish-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A staged artifact the check refuses never reaches the output
    /// path: the previous output keeps its bytes and the staged file is
    /// gone. One the check accepts replaces the output.
    #[test]
    fn a_refused_artifact_is_never_published() {
        let dir = scratch("refused");
        let (staged, output) = (dir.join("set.staged"), dir.join("set.onelf"));
        std::fs::write(&output, b"previous").unwrap();
        std::fs::write(&staged, b"unverified").unwrap();
        let err = publish_checked(&staged, &output, |_| {
            Err(std::io::Error::other("index mismatch"))
        })
        .unwrap_err();
        assert!(err.to_string().contains("index mismatch"), "{err}");
        assert_eq!(std::fs::read(&output).unwrap(), b"previous");
        assert!(!staged.exists(), "the refused artifact stays behind");

        std::fs::write(&staged, b"verified").unwrap();
        let answer = publish_checked(&staged, &output, |p| {
            assert_eq!(std::fs::read(p).unwrap(), b"verified");
            Ok("hash".to_string())
        })
        .unwrap();
        assert_eq!(answer, "hash");
        assert_eq!(std::fs::read(&output).unwrap(), b"verified");
        assert!(!staged.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
