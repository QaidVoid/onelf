//! Optional end-to-end smoke test: build a fully-native i686 package and run it
//! inside a real 32-bit (i386) container.
//!
//! This is heavy (it cross-compiles onelf for `i686-unknown-linux-musl` and
//! pulls a container image), so it is gated behind `ONELF_SMOKE_I386=1` and
//! otherwise skipped. The actual work lives in `scripts/smoke-i386.sh`, which
//! self-skips (exit 0) when its prerequisites -- the bootlin i686 toolchains,
//! podman, and the rust musl target -- are absent.

use std::path::Path;
use std::process::Command;

#[test]
fn native_i686_package_runs_in_i386_container() {
    if std::env::var_os("ONELF_SMOKE_I386").is_none() {
        eprintln!("skipping: set ONELF_SMOKE_I386=1 to run the i386 container smoke test");
        return;
    }
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/smoke-i386.sh");
    let status = Command::new("bash")
        .arg(&script)
        .status()
        .expect("failed to run scripts/smoke-i386.sh");
    assert!(status.success(), "scripts/smoke-i386.sh reported failure");
}
