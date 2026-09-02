//! `onelf sysroot`: obtain and inspect a pinned sysroot.

use std::io::{self, Write};
use std::path::Path;

use onelf_sysroot::{Database, archive};

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
