//! `onelf sysroot`: obtain and inspect a pinned sysroot.

use std::io::{self, Write};
use std::path::Path;

use onelf_sysroot::{Database, archive};

use crate::bundle::elf::audit_unbundled_needs;
use crate::bundle::remove_dangling_links_beside;
use crate::pack::{HostLibs, PackOptions};

/// Materialize the rootfs archive at `source`, a local path or an
/// `https://` URL, into `dir`.
pub fn fetch(source: &str, dir: &Path) -> io::Result<()> {
    if source.starts_with("http://") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a sysroot is executable code; fetch it over https:// only",
        ));
    }
    if let Some(rest) = source.strip_prefix("https://") {
        let name = rest.rsplit('/').next().unwrap_or("sysroot.tar");
        let staged = dir.with_extension(format!("download-{}", std::process::id()));
        let downloaded = download(source, &staged);
        let result = downloaded.and_then(|()| {
            eprintln!("Materializing {name} into {}", dir.display());
            archive::materialize(&staged, dir)
        });
        let _ = std::fs::remove_file(&staged);
        return result;
    }
    let archive_path = Path::new(source);
    eprintln!(
        "Materializing {} into {}",
        archive_path.display(),
        dir.display()
    );
    archive::materialize(archive_path, dir)
}

fn download(url: &str, into: &Path) -> io::Result<()> {
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

/// Pack the GL tree at `dir` into `output` and print the hash to pin.
///
/// The tree has to be self-contained apart from glibc, which the package
/// that uses it carries, and the driver families it exists to provide.
/// Anything else it needs and does not carry is an error here rather
/// than a silent failure on a host with nothing to fall back on.
pub fn pack_gl(dir: &Path, output: &Path, runtime: &[u8]) -> io::Result<()> {
    let entry = dir.join(GL_ENTRYPOINT);
    if !entry.exists() {
        if let Some(parent) = entry.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &entry,
            "#!/bin/sh\necho 'onelf GL build; not meant to be run'\n",
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
                "lib/{name}: a GL build takes glibc from the package that uses it; leave the glibc package's files out"
            ),
        ));
    }

    let mut findings = audit_unbundled_needs(dir, Path::new("lib"));
    for (_, libs) in &mut findings {
        libs.retain(|s| {
            !onelf_format::drivers::DRIVER_FAMILIES
                .iter()
                .any(|p| s.starts_with(p))
                && !onelf_format::resolve::is_libc_family(s)
        });
    }
    findings.retain(|(_, libs)| !libs.is_empty());
    // A closure carries bindings for stacks outside it, a Python module
    // of util-linux say. Nothing loads those from a GL build, so they go,
    // by name, rather than failing the build.
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

    crate::pack::pack(
        &PackOptions {
            directory: dir.to_path_buf(),
            output: output.to_path_buf(),
            command: GL_ENTRYPOINT.to_string(),
            name: Some("onelf-gl".to_string()),
            entrypoints: Vec::new(),
            default_entrypoint: None,
            lib_dirs: vec!["auto".to_string()],
            level: 12,
            block_size: crate::compress::DEFAULT_BLOCK_SIZE,
            use_dict: false,
            no_compress: false,
            memfd: Some(false),
            working_dir: onelf_format::WorkingDir::Inherit,
            host_libs: HostLibs::Never,
            cache: false,
            update_url: None,
            embed_updater: false,
            update_key: None,
            exclude: Vec::new(),
            package_info: None,
            mtime: Some(0),
            env: Vec::new(),
            preload: Vec::new(),
            needs_setuid: false,
        },
        runtime,
    )?;

    let hash = hash_file(output)?;
    println!("blake3 = \"{hash}\"");
    Ok(())
}

/// Lowercase hex BLAKE3 of the file at `path`, streamed.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(hasher.finalize().to_hex().to_string())
}
