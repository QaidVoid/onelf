mod cache;
mod loader;

fn main() {
    let mut pkg = match loader::load() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("onelf-rt: failed to load package: {e}");
            std::process::exit(1);
        }
    };

    let pkg_dir = match cache::ensure_extracted(&mut pkg) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("onelf-rt: extraction failed: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("onelf-rt: extracted to {}", pkg_dir.display());
}
