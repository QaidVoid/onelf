//! Pinned distro fixtures: pack and run the documented application kinds
//! inside Ubuntu 24.04, Alpine 3.20 and Fedora 44 container images, and
//! prove what the static musl runtime guarantees there.
//!
//! The images build from `ci/fixtures/Dockerfile.*`: base tags pinned by
//! digest, package lists installed only at image-prep time. Each test
//! bind-mounts the repository read-only, compiles a fixture application
//! with the image's own toolchain, packs it with the workspace's static
//! musl `onelf`, and runs the package in the same image or a sibling
//! one. onelf itself never needs the network: every packed library comes
//! from the image, the recipes carry no pins, and the musl binary builds
//! offline from the checked-out workspace.
//!
//! The fixture matrix:
//!
//! * each distro builds, packs and runs the CLI, GTK, Qt, script plus
//!   helper, and environment-clearing re-exec fixtures, asserting the
//!   documented output of each,
//! * the GTK and Qt packages also run on a bare base image of a foreign
//!   libc that has no GTK or Qt installed, once cleanly and once under
//!   hostile inherited GStreamer, Qt, GTK and GIO search paths plus a
//!   conflicting `/etc/asound.conf`, so their output proves the bundle
//!   answered,
//! * a package built on one distro runs on the other two, with the
//!   compiled-in marker naming the distro that produced it,
//! * the slim and update-feature runtimes are static musl (no
//!   interpreter segment, no `DT_NEEDED`), advertise selection-view and
//!   additionally store-import through the doctor report, and run on all
//!   three distros,
//! * a glibc application packed with the musl runtime and a
//!   host-libs-never package prints its bundled glibc version on Ubuntu,
//!   on Fedora and on Alpine, so Alpine's musl never answers, and a musl
//!   application packed the same way prints musl on a glibc host.
//!
//! Every container runs with `--network none` and as the invoking user,
//! and each test prints the image ID of every image it runs, so a log
//! names exactly what was exercised.
//!
//! Capability skips are accounted, not silent: without a container
//! engine (docker, or podman) or without `ONELF_DISTRO_FIXTURES=1` the
//! tests announce `skip [docker]` and claim nothing. A job that promises
//! the capability through `ONELF_REQUIRED_CAPS=docker` fails instead, so
//! CI cannot go green on a skip.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};

/// The distros the fixtures pin, in the order tests report them.
const DISTROS: [&str; 3] = ["ubuntu-24.04", "alpine-3.20", "fedora-44"];

/// The workspace's own `onelf` CLI, built for the host.
fn onelf() -> &'static str {
    env!("CARGO_BIN_EXE_onelf")
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

/// The static musl `onelf` binary, at its cargo target path.
fn musl_onelf_path() -> PathBuf {
    target_dir()
        .join("x86_64-unknown-linux-musl")
        .join("debug")
        .join("onelf")
}

/// Whether a command runs and exits successfully.
fn runs(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The capabilities the surrounding job promises, from
/// `ONELF_REQUIRED_CAPS` (comma-separated names). Empty when unset.
fn required_caps() -> &'static HashSet<String> {
    static CAPS: OnceLock<HashSet<String>> = OnceLock::new();
    CAPS.get_or_init(|| {
        std::env::var("ONELF_REQUIRED_CAPS")
            .unwrap_or_default()
            .split(',')
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect()
    })
}

/// Whether the surrounding job claims capability `cap` as required: a
/// skip of a claimed capability means the environment lied about what it
/// provides, and the job must fail rather than report the coverage.
fn required(cap: &str) -> bool {
    required_caps().contains(cap)
}

/// Announce that a test is skipping for lack of the capability `cap`.
///
/// Same convention as the pipeline suite: a skip of a capability a job
/// claims through `ONELF_REQUIRED_CAPS` panics, and under `CI` with no
/// declared capabilities every skip is a failure. Otherwise the skip is
/// announced on stderr and the caller returns.
fn skip(cap: &str, reason: &str) {
    if required(cap) {
        panic!("required capability '{cap}' is unavailable: {reason}");
    }
    let declared = std::env::var("ONELF_REQUIRED_CAPS").is_ok_and(|v| !v.trim().is_empty());
    if !declared && std::env::var_os("CI").is_some_and(|v| !v.is_empty()) {
        panic!("skip under CI: {reason}");
    }
    eprintln!("skip [{cap}]: {reason}");
}

/// A container engine the fixtures may drive: docker when present, or
/// podman, or whatever `ONELF_CONTAINER_ENGINE` names.
struct Engine(String);

impl Engine {
    fn run(&self, args: &[String]) -> Output {
        Command::new(&self.0)
            .args(args)
            .output()
            .expect("spawn container engine")
    }
}

/// Whether the engine binary answers `info`, which for both engines
/// means a working daemon or machine, not merely an installed binary.
fn engine_answers(bin: &str) -> bool {
    Command::new(bin)
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether the operator asked for the fixtures to run. Off by default so
/// an ordinary local `cargo test` never builds gigabytes of images; the
/// CI distro job turns it on beside `ONELF_REQUIRED_CAPS`.
fn fixtures_enabled() -> bool {
    std::env::var("ONELF_DISTRO_FIXTURES")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Resolve the engine and the operator opt-in, or announce the skip that
/// keeps the local suite untouched and return `None`.
fn gate() -> Option<Engine> {
    let named = std::env::var("ONELF_CONTAINER_ENGINE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let engine = match named {
        Some(bin) => {
            if engine_answers(&bin) {
                Some(bin)
            } else {
                skip(
                    "docker",
                    &format!("ONELF_CONTAINER_ENGINE={bin} does not answer `info`"),
                );
                return None;
            }
        }
        None => ["docker", "podman"]
            .into_iter()
            .find(|bin| engine_answers(bin))
            .map(String::from),
    };
    let Some(engine) = engine else {
        skip(
            "docker",
            "no container engine with a working daemon: docker and podman both unavailable",
        );
        return None;
    };
    if !fixtures_enabled() {
        skip(
            "docker",
            &format!(
                "engine {engine} is available but ONELF_DISTRO_FIXTURES is not set to 1, \
                 so the image builds stay opt-in"
            ),
        );
        return None;
    }
    Some(Engine(engine))
}

/// The static musl `onelf` the containers drive, built offline once per
/// process.
fn musl_onelf() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = musl_onelf_path();
        if !out.is_file() {
            let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
            cmd.args([
                "build",
                "-p",
                "onelf",
                "--locked",
                "--offline",
                "--target",
                "x86_64-unknown-linux-musl",
            ]);
            cmd.current_dir(workspace_root());
            // The C dependencies of the musl build take this compiler
            // when the musl-gcc wrapper itself is not on PATH under that
            // name; the workspace's runtime build searches the same
            // names.
            if let Some(cc) = musl_cc() {
                cmd.env("CC_x86_64_unknown_linux_musl", &cc);
            }
            let o = cmd.output().expect("spawn cargo");
            assert!(
                o.status.success(),
                "static musl onelf build failed:\n{}\n{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
        }
        assert!(
            out.is_file(),
            "no static musl onelf at {} after the build",
            out.display()
        );
        out.clone()
    })
    .clone()
}

/// A musl gcc for the C dependencies of the musl build, by the same
/// names the workspace's runtime build searches. `None` leaves the
/// choice to cargo, whose default for the musl target is `musl-gcc`.
fn musl_cc() -> Option<String> {
    ["x86_64-linux-musl-gcc", "musl-gcc"]
        .into_iter()
        .find(|cc| runs(cc))
        .map(String::from)
}

/// A test's scratch directory, shared with the containers at `/work` and
/// removed when the test ends however it ends.
struct Workdir(PathBuf);

impl Workdir {
    fn new(tag: &str) -> Workdir {
        let d = std::env::temp_dir().join(format!(
            "onelf-distro-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(d.join("apps")).unwrap();
        Workdir(d)
    }

    fn app(&self, name: &str) -> PathBuf {
        self.0.join("apps").join(name)
    }
}

impl std::ops::Deref for Workdir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build one distro's fixture image once per process, from its pinned
/// Dockerfile. Nothing is copied into the image: the applications
/// compile inside the container at test time.
fn image(engine: &Engine, distro: &str) -> String {
    static BUILT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let built = BUILT.get_or_init(|| Mutex::new(HashSet::new()));
    let mut built = built.lock().unwrap();
    let tag = format!("onelf-fixture:{distro}");
    if built.insert(distro.to_string()) {
        let dir = workspace_root().join("ci/fixtures");
        let dockerfile = dir.join(format!("Dockerfile.{distro}"));
        let args = vec![
            "build".to_string(),
            "-f".to_string(),
            dockerfile.to_string_lossy().into_owned(),
            "-t".to_string(),
            tag.clone(),
            dir.to_string_lossy().into_owned(),
        ];
        let o = engine.run(&args);
        assert!(
            o.status.success(),
            "fixture image {distro} failed to build:\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
        record_image_id(engine, &tag);
    }
    tag
}

/// The bare base image a distro's fixture Dockerfile starts `FROM`,
/// pulled once per process. It carries none of the fixture packages, so
/// a package that runs there brought its own libraries.
fn bare_image(engine: &Engine, distro: &str) -> String {
    static PULLED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let pulled = PULLED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut pulled = pulled.lock().unwrap();
    let dockerfile = workspace_root().join(format!("ci/fixtures/Dockerfile.{distro}"));
    let text = std::fs::read_to_string(&dockerfile)
        .unwrap_or_else(|e| panic!("read {}: {e}", dockerfile.display()));
    let base = text
        .lines()
        .find_map(|l| l.strip_prefix("FROM "))
        .unwrap_or_else(|| panic!("{} has no FROM line", dockerfile.display()))
        .trim()
        .to_string();
    if pulled.insert(base.clone()) {
        let o = engine.run(&["pull".to_string(), base.clone()]);
        assert!(
            o.status.success(),
            "pulling the bare image {base} failed:\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
        record_image_id(engine, &base);
    }
    base
}

/// Print the engine's image ID for `reference`, so the test output names
/// the exact image the fixtures ran.
fn record_image_id(engine: &Engine, reference: &str) {
    let o = engine.run(&[
        "image".to_string(),
        "inspect".to_string(),
        "--format".to_string(),
        "{{.Id}}".to_string(),
        reference.to_string(),
    ]);
    assert!(
        o.status.success(),
        "inspecting image {reference} failed:\n{}",
        String::from_utf8_lossy(&o.stderr)
    );
    eprintln!(
        "fixture image {reference}: {}",
        String::from_utf8_lossy(&o.stdout).trim()
    );
}

/// One `run` in a fixture container: the repository at `/repo` and the
/// cargo target directory at `/onelf-target`, both read-only, the
/// scratch tree at `/work`, and the static musl onelf at `$ONELF_BIN`.
/// The container has no network and runs as the owner of the scratch
/// tree with `HOME=/work`, so everything it writes stays removable by
/// the test. `extra_args` go to the engine before the image.
fn container(
    engine: &Engine,
    tag: &str,
    scratch: &Workdir,
    extra_env: &[(&str, String)],
    extra_args: &[String],
    script: &str,
) -> Output {
    let onelf_bin = format!(
        "/onelf-target/{}",
        musl_onelf()
            .strip_prefix(target_dir())
            .expect("musl onelf under the target directory")
            .to_string_lossy()
    );
    let mut cmd = Command::new(&engine.0);
    cmd.args(["run", "--rm", "--network", "none"]);
    cmd.args(user_args(engine, scratch));
    cmd.arg("-v")
        .arg(format!("{}:/repo:ro", workspace_root().to_string_lossy()));
    cmd.arg("-v").arg(format!(
        "{}:/onelf-target:ro",
        target_dir().to_string_lossy()
    ));
    cmd.arg("-v")
        .arg(format!("{}:/work", scratch.to_string_lossy()));
    cmd.arg("-e").arg(format!("ONELF_BIN={onelf_bin}"));
    cmd.arg("-e").arg("HOME=/work");
    for (key, value) in extra_env {
        cmd.arg("-e").arg(format!("{key}={value}"));
    }
    cmd.args(extra_args);
    cmd.arg(tag).arg("sh").arg("-c").arg(script);
    cmd.output().expect("spawn container run")
}

/// Run the container as the owner of the scratch tree. Rootless podman
/// maps that user through `keep-id`; docker takes the numeric ids.
fn user_args(engine: &Engine, scratch: &Workdir) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;

    let podman = Path::new(&engine.0)
        .file_name()
        .is_some_and(|name| name == "podman");
    if podman {
        return vec!["--userns=keep-id".to_string()];
    }
    let meta = std::fs::metadata(&scratch.0)
        .unwrap_or_else(|e| panic!("stat {}: {e}", scratch.0.display()));
    vec![
        "--user".to_string(),
        format!("{}:{}", meta.uid(), meta.gid()),
    ]
}

fn container_simple(engine: &Engine, tag: &str, scratch: &Workdir, script: &str) -> Output {
    container(engine, tag, scratch, &[], &[], script)
}

/// Run a container command and fail with the transcript when it exits
/// nonzero.
fn must(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed (exit {:?}):\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn transcript(out: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Compile the named fixture applications inside the image, with the
/// image's own toolchain and libraries.
fn assemble(engine: &Engine, tag: &str, scratch: &Workdir, distro: &str, apps: &[&str]) {
    let script = format!(
        "sh /repo/ci/fixtures/assemble.sh {distro} {}",
        apps.join(" ")
    );
    let out = container_simple(engine, tag, scratch, &script);
    must(&out, &format!("assembling fixtures in {distro}"));
    assert!(
        stdout_of(&out).contains(&format!("BUILD_OK {distro}")),
        "assembler did not report success:\n{}",
        transcript(&out)
    );
}

/// Write a recipe into one assembled application directory on the
/// scratch tree.
fn write_recipe(scratch: &Workdir, app: &str, body: &str) {
    let path = scratch.app(app).join("onelf.toml");
    std::fs::write(&path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Pack one application directory with `onelf build` inside the image.
/// The package lands beside the recipe, named after the recipe's `name`.
/// `patchelf` enables the onelf-env constructor injection the re-exec
/// fixture needs.
fn pack(engine: &Engine, tag: &str, scratch: &Workdir, app: &str, with_patchelf: bool) -> Output {
    let script = format!("\"$ONELF_BIN\" build /work/apps/{app}");
    let env: Vec<(&str, String)> = if with_patchelf {
        vec![("ONELF_PATCHELF", "patchelf".to_string())]
    } else {
        Vec::new()
    };
    container(engine, tag, scratch, &env, &[], &script)
}

/// Run a packed package in an image, with only the environment the image
/// gives a launch plus a scratch `HOME` for the extraction cache.
fn run_pkg(engine: &Engine, tag: &str, scratch: &Workdir, pkg_rel: &str, args: &[&str]) -> Output {
    let script = format!("HOME=/work {} {}", in_work(pkg_rel), args.join(" "));
    container_simple(engine, tag, scratch, script.trim_end())
}

/// Run a packed package with hostile inherited framework search paths
/// that all point at junk under `/work/hostile`, and with that tree's
/// conflicting ALSA snippet mounted over `/etc/asound.conf`. A package
/// that bundles its frameworks must not let any of them answer.
fn run_pkg_hostile(engine: &Engine, tag: &str, scratch: &Workdir, pkg_rel: &str) -> Output {
    let hostile = write_hostile_tree(scratch);
    let dir = |rel: &str| format!("/work/hostile/{rel}");
    let env = [
        ("GST_PLUGIN_PATH", dir("gstreamer-1.0")),
        ("GST_PLUGIN_PATH_1_0", dir("gstreamer-1.0")),
        ("GST_PLUGIN_SYSTEM_PATH", dir("gstreamer-1.0")),
        ("GST_PLUGIN_SYSTEM_PATH_1_0", dir("gstreamer-1.0")),
        ("GST_PLUGIN_SCANNER", dir("gst-plugin-scanner")),
        ("GST_PLUGIN_SCANNER_1_0", dir("gst-plugin-scanner")),
        ("QT_PLUGIN_PATH", dir("qt/plugins")),
        ("QT_QPA_PLATFORM_PLUGIN_PATH", dir("qt/plugins/platforms")),
        ("GTK_PATH", dir("gtk-3.0")),
        ("GTK_IM_MODULE_FILE", dir("immodules.cache")),
        ("GDK_PIXBUF_MODULE_FILE", dir("loaders.cache")),
        ("GIO_MODULE_DIR", dir("gio/modules")),
    ];
    let mount = vec![
        "-v".to_string(),
        format!(
            "{}:/etc/asound.conf:ro",
            hostile.join("asound.conf").display()
        ),
    ];
    container(engine, tag, scratch, &env, &mount, &in_work(pkg_rel))
}

/// The junk tree the hostile variables name: plugin files that are not
/// ELF objects, a scanner that reports itself if anything runs it,
/// unparsable module caches, and an ALSA default naming a PipeWire
/// backend no fixture image has.
fn write_hostile_tree(scratch: &Workdir) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let root = scratch.join("hostile");
    let junk = b"not an ELF object\n";
    for rel in [
        "gstreamer-1.0/libgstcoreelements.so",
        "qt/plugins/platforms/libqxcb.so",
        "qt/plugins/platforms/libqoffscreen.so",
        "gtk-3.0/3.0.0/immodules/im-hostile.so",
        "gio/modules/libgiohostile.so",
    ] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, junk).unwrap();
    }
    std::fs::write(root.join("immodules.cache"), "}{ hostile\n").unwrap();
    std::fs::write(root.join("loaders.cache"), "}{ hostile\n").unwrap();
    let scanner = root.join("gst-plugin-scanner");
    std::fs::write(
        &scanner,
        "#!/bin/sh\necho HOSTILE_SCANNER_RAN >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&scanner, PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("asound.conf"),
        "pcm.!default {\n    type pipewire\n}\nctl.!default {\n    type pipewire\n}\n",
    )
    .unwrap();
    root
}

/// Assert a hostile-path run behaved like a clean one: the expected
/// marker, and no sign that the junk scanner ran.
fn assert_hostile_run(out: &Output, prefix: &str, what: &str) {
    must(out, what);
    assert!(
        stdout_of(out).starts_with(prefix),
        "{what}: wrong output under hostile inherited paths:\n{}",
        transcript(out)
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("HOSTILE_SCANNER_RAN"),
        "{what}: the inherited scanner ran:\n{}",
        transcript(out)
    );
}

/// The libc family a distro is not: the foreign bare image a package
/// built there runs on to prove it carries its own libraries.
fn foreign_distro(distro: &str) -> &'static str {
    if distro.starts_with("alpine") {
        "ubuntu-24.04"
    } else {
        "alpine-3.20"
    }
}

/// The package file an `onelf build` of recipe `name` produced.
fn pkg_path(scratch: &Workdir, app: &str, name: &str) -> PathBuf {
    scratch.app(app).join(format!("{name}.onelf"))
}

/// A scratch-relative path as a container sees it under `/work`.
fn in_work(rel: &str) -> String {
    format!("/work/{rel}")
}

/// `file`, `readelf` and `ldd` transcripts of a packed package, taken
/// inside the image so the answers come from the pinned distro's own
/// binutils. `file` and `readelf` must succeed. `ldd` exits nonzero on
/// a static object, so its exit status is recorded rather than checked,
/// and [`assert_static_musl`] requires its static verdict instead.
fn static_report(engine: &Engine, tag: &str, scratch: &Workdir, pkg_rel: &str) -> String {
    let pkg = in_work(pkg_rel);
    let script = format!(
        "set -e; file {pkg}; echo ---; readelf -lW {pkg}; echo ---; readelf -dW {pkg}; echo ---; \
         status=0; ldd {pkg} 2>&1 || status=$?; echo \"ldd exit $status\""
    );
    let out = container_simple(engine, tag, scratch, &script);
    must(&out, &format!("inspecting {pkg_rel}"));
    stdout_of(&out)
}

/// Assert the report shows a static musl runtime: a static link line, no
/// interpreter segment, no dynamic entries naming a library.
fn assert_static_musl(report: &str, what: &str) {
    assert!(
        report.contains("Program Headers:"),
        "{what}: readelf printed no program headers, so the absence checks prove nothing:\n{report}"
    );
    assert!(
        report.contains("not a dynamic executable") || report.contains("statically linked"),
        "{what}: ldd did not report a static object:\n{report}"
    );
    assert!(
        report.contains("static-pie linked") || report.contains("statically linked"),
        "{what}: `file` does not report a static link:\n{report}"
    );
    assert!(
        !report.contains("INTERP"),
        "{what}: the packed runtime carries a PT_INTERP segment:\n{report}"
    );
    assert!(
        !report.contains("(NEEDED)"),
        "{what}: the packed runtime carries DT_NEEDED entries:\n{report}"
    );
}

/// Decode the capability note of a packed package the way the loader
/// does, and assert it advertises exactly `expected`.
fn assert_note(pkg: &Path, expected: onelf_format::elf::RuntimeCapabilities, what: &str) {
    let data = std::fs::read(pkg).unwrap_or_else(|e| panic!("read {}: {e}", pkg.display()));
    match onelf_format::elf::probe_capability_note(&data) {
        Ok(onelf_format::elf::RuntimeNote::Present {
            metadata_version,
            capabilities,
        }) => {
            assert_eq!(
                metadata_version,
                onelf_format::elf::RUNTIME_METADATA_VERSION,
                "{what}: unexpected metadata version"
            );
            assert_eq!(capabilities, expected, "{what}: wrong advertisement");
        }
        other => panic!("{what}: capability note did not decode: {other:?}"),
    }
}

/// The `onelf info` line stating the package's host-libs policy.
fn assert_info_host_libs(pkg: &Path, policy: &str, what: &str) {
    let out = Command::new(onelf())
        .args(["info", &pkg.to_string_lossy()])
        .output()
        .expect("spawn onelf info");
    must(&out, &format!("onelf info {}", pkg.display()));
    let text = stdout_of(&out);
    let line = text
        .lines()
        .find(|l| l.starts_with("Host libs:"))
        .unwrap_or_else(|| panic!("{what}: info has no Host libs line:\n{text}"));
    assert!(
        line.ends_with(policy),
        "{what}: info reports '{line}', expected policy {policy}"
    );
}

/// A fixture recipe: the shared `[package]` table, any extra
/// `[package]` keys, a cheap compression profile, and any further
/// tables (`[env]`, `[update]`) in order. `cache = true` gives every
/// package the extraction fallback, so a launch works in a container
/// whose kernel grants no mounts; `mtime = 0` pins the bytes.
fn recipe(name: &str, command: &str, package_keys: &[&str], tables: &[&str]) -> String {
    let mut body =
        format!("[package]\nname = \"{name}\"\ncommand = \"{command}\"\ncache = true\nmtime = 0");
    for key in package_keys {
        body.push('\n');
        body.push_str(key);
    }
    body.push_str("\n\n[compression]\nlevel = 1");
    for table in tables {
        body.push_str("\n\n");
        body.push_str(table);
    }
    body
}

/// The build, pack and run of every application kind on one pinned
/// distro, asserting each documented output.
fn app_kinds_run_on(distro: &'static str) {
    let Some(engine) = gate() else { return };
    let tag = image(&engine, distro);
    let td = Workdir::new(distro);

    assemble(
        &engine,
        &tag,
        &td,
        distro,
        &["cli", "gtk", "qt", "script", "env"],
    );

    // CLI: build, pack, run, and the marker names the distro.
    write_recipe(&td, "cli", &recipe("onelf-distro-cli", "bin/cli", &[], &[]));
    must(
        &pack(&engine, &tag, &td, "cli", false),
        &format!("{distro}: packing the CLI fixture"),
    );
    let out = run_pkg(&engine, &tag, &td, "apps/cli/onelf-distro-cli.onelf", &[]);
    must(&out, &format!("{distro}: running the packed CLI fixture"));
    assert!(
        stdout_of(&out).contains(&format!("CLI_OK {distro}")),
        "{distro}: CLI fixture printed the wrong marker:\n{}",
        transcript(&out)
    );

    // GTK: the bundled GTK 3 chain answers headlessly with its version.
    write_recipe(&td, "gtk", &recipe("onelf-distro-gtk", "bin/gtk", &[], &[]));
    must(
        &pack(&engine, &tag, &td, "gtk", false),
        &format!("{distro}: packing the GTK fixture"),
    );
    let out = run_pkg(&engine, &tag, &td, "apps/gtk/onelf-distro-gtk.onelf", &[]);
    must(&out, &format!("{distro}: running the packed GTK fixture"));
    assert!(
        stdout_of(&out).starts_with("GTK_OK 3."),
        "{distro}: GTK fixture did not report GTK 3 from the bundle:\n{}",
        transcript(&out)
    );

    // Qt: the bundled Qt 6 core answers headlessly through qVersion().
    write_recipe(&td, "qt", &recipe("onelf-distro-qt", "bin/qt", &[], &[]));
    must(
        &pack(&engine, &tag, &td, "qt", false),
        &format!("{distro}: packing the Qt fixture"),
    );
    let out = run_pkg(&engine, &tag, &td, "apps/qt/onelf-distro-qt.onelf", &[]);
    must(&out, &format!("{distro}: running the packed Qt fixture"));
    assert!(
        stdout_of(&out).starts_with("QT_OK 6."),
        "{distro}: Qt fixture did not report Qt 6 from the bundle:\n{}",
        transcript(&out)
    );

    // GTK and Qt again on a bare image of the other libc family, which
    // has neither toolkit nor this libc installed, first cleanly and then
    // under hostile inherited framework paths.
    let foreign = foreign_distro(distro);
    let bare = bare_image(&engine, foreign);
    for (rel, prefix) in [
        ("apps/gtk/onelf-distro-gtk.onelf", "GTK_OK 3."),
        ("apps/qt/onelf-distro-qt.onelf", "QT_OK 6."),
    ] {
        let out = run_pkg(&engine, &bare, &td, rel, &[]);
        must(&out, &format!("{distro}: {rel} on bare {foreign}"));
        assert!(
            stdout_of(&out).starts_with(prefix),
            "{distro}: {rel} did not answer from its bundle on bare {foreign}:\n{}",
            transcript(&out)
        );
        let out = run_pkg_hostile(&engine, &bare, &td, rel);
        assert_hostile_run(
            &out,
            prefix,
            &format!("{distro}: {rel} on bare {foreign} with hostile paths"),
        );
    }

    // Script and helper: a shell entrypoint execs a bundled companion.
    write_recipe(
        &td,
        "script",
        &recipe("onelf-distro-script", "bin/run", &[], &[]),
    );
    must(
        &pack(&engine, &tag, &td, "script", false),
        &format!("{distro}: packing the script fixture"),
    );
    let out = run_pkg(
        &engine,
        &tag,
        &td,
        "apps/script/onelf-distro-script.onelf",
        &[],
    );
    must(
        &out,
        &format!("{distro}: running the packed script fixture"),
    );
    let text = stdout_of(&out);
    assert!(
        text.contains("SCRIPT_OK") && text.contains("HELPER_OK"),
        "{distro}: script fixture lost the script or the helper:\n{}",
        transcript(&out)
    );

    // Environment-clearing re-exec: clearenv() plus an exec of
    // /proc/self/exe must still find the packaged [env] records applied,
    // which needs the bundled constructor, which needs patchelf at pack
    // time.
    write_recipe(
        &td,
        "env",
        &recipe(
            "onelf-distro-env",
            "bin/harness",
            &[],
            &["[env]\nONELF_IT_VAR = \"survived\""],
        ),
    );
    let packed = pack(&engine, &tag, &td, "env", true);
    let log = transcript(&packed);
    must(&packed, &format!("{distro}: packing the re-exec fixture"));
    assert!(
        log.contains("Injected onelf-env"),
        "{distro}: expected the onelf-env constructor injection:\n{log}"
    );
    let out = run_pkg(&engine, &tag, &td, "apps/env/onelf-distro-env.onelf", &[]);
    must(
        &out,
        &format!("{distro}: running the packed re-exec fixture"),
    );
    assert!(
        stdout_of(&out).contains("ENV_PASS v=[survived]"),
        "{distro}: the packaged env did not survive clearenv() and re-exec:\n{}",
        transcript(&out)
    );
}

/// The full matrix on each pinned distro.
#[test]
fn fixture_app_kinds_build_pack_and_run_on_ubuntu_24_04() {
    app_kinds_run_on("ubuntu-24.04");
}

#[test]
fn fixture_app_kinds_build_pack_and_run_on_alpine_3_20() {
    app_kinds_run_on("alpine-3.20");
}

#[test]
fn fixture_app_kinds_build_pack_and_run_on_fedora_44() {
    app_kinds_run_on("fedora-44");
}

/// Build on one distro, run on another: the compiled-in marker names the
/// distro that packed the package, so a match proves the whole bundle
/// carried over, libraries included.
#[test]
fn packages_run_across_distro_lines() {
    let Some(engine) = gate() else { return };

    // The Ubuntu-built package on the musl distro and on Fedora.
    let ubuntu = image(&engine, "ubuntu-24.04");
    let td = Workdir::new("cross-ubuntu");
    assemble(&engine, &ubuntu, &td, "ubuntu-24.04", &["cli"]);
    write_recipe(&td, "cli", &recipe("onelf-distro-cli", "bin/cli", &[], &[]));
    must(
        &pack(&engine, &ubuntu, &td, "cli", false),
        "cross: packing on Ubuntu",
    );
    for host in ["alpine-3.20", "fedora-44"] {
        let host_tag = image(&engine, host);
        let out = run_pkg(
            &engine,
            &host_tag,
            &td,
            "apps/cli/onelf-distro-cli.onelf",
            &[],
        );
        must(&out, &format!("cross: Ubuntu package on {host}"));
        assert!(
            stdout_of(&out).contains("CLI_OK ubuntu-24.04"),
            "cross: the Ubuntu package printed the wrong marker on {host}:\n{}",
            transcript(&out)
        );
    }

    // The Alpine-built package on a glibc distro.
    let alpine = image(&engine, "alpine-3.20");
    let td = Workdir::new("cross-alpine");
    assemble(&engine, &alpine, &td, "alpine-3.20", &["cli"]);
    write_recipe(&td, "cli", &recipe("onelf-distro-cli", "bin/cli", &[], &[]));
    must(
        &pack(&engine, &alpine, &td, "cli", false),
        "cross: packing on Alpine",
    );
    let fedora = image(&engine, "fedora-44");
    let out = run_pkg(
        &engine,
        &fedora,
        &td,
        "apps/cli/onelf-distro-cli.onelf",
        &[],
    );
    must(&out, "cross: Alpine package on Fedora");
    assert!(
        stdout_of(&out).contains("CLI_OK alpine-3.20"),
        "cross: the Alpine package printed the wrong marker on Fedora:\n{}",
        transcript(&out)
    );
}

/// The slim and update-feature runtimes: static musl by `file` and
/// `readelf`, advertised capabilities by the note decoder and by the
/// doctor report, and portable across all three distros.
#[test]
fn slim_and_update_runtimes_are_static_musl_and_advertise_their_capabilities() {
    let Some(engine) = gate() else { return };
    let ubuntu = image(&engine, "ubuntu-24.04");
    let td = Workdir::new("runtimes");
    assemble(&engine, &ubuntu, &td, "ubuntu-24.04", &["cli"]);

    // The same application twice: one recipe that carries selection
    // records on the slim runtime, one that also names an update URL and
    // so embeds the update-feature runtime.
    let script =
        "cp -a /work/apps/cli /work/apps/cli-slim && cp -a /work/apps/cli /work/apps/cli-update";
    must(
        &container_simple(&engine, &ubuntu, &td, script),
        "copying the CLI fixture for the two runtimes",
    );
    write_recipe(
        &td,
        "cli-slim",
        &recipe("onelf-distro-slim", "bin/cli", &["sets = []"], &[]),
    );
    write_recipe(
        &td,
        "cli-update",
        &recipe(
            "onelf-distro-update",
            "bin/cli",
            &["sets = []"],
            &["[update]\nurl = \"https://fixture.invalid/onelf-distro-update.onelf\""],
        ),
    );
    must(
        &pack(&engine, &ubuntu, &td, "cli-slim", false),
        "runtimes: packing slim",
    );
    must(
        &pack(&engine, &ubuntu, &td, "cli-update", false),
        "runtimes: packing update",
    );

    let slim = pkg_path(&td, "cli-slim", "onelf-distro-slim");
    let update = pkg_path(&td, "cli-update", "onelf-distro-update");

    // The note decoder packaging and the loader share, on the host: slim
    // advertises selection-view, update adds store-import.
    let view = onelf_format::elf::RuntimeCapabilities::SELECTION_VIEW;
    let both = view.union(onelf_format::elf::RuntimeCapabilities::STORE_IMPORT);
    assert_note(&slim, view, "slim runtime note");
    assert_note(&update, both, "update runtime note");

    // Static musl, per the pinned distro's own binutils.
    let report = static_report(
        &engine,
        &ubuntu,
        &td,
        "apps/cli-slim/onelf-distro-slim.onelf",
    );
    assert_static_musl(&report, "slim runtime");
    let report = static_report(
        &engine,
        &ubuntu,
        &td,
        "apps/cli-update/onelf-distro-update.onelf",
    );
    assert_static_musl(&report, "update runtime");

    // The doctor report states the agreement between what the package's
    // records require and what its embedded runtime advertises.
    let out = run_pkg(
        &engine,
        &ubuntu,
        &td,
        "apps/cli-slim/onelf-distro-slim.onelf",
        &["--onelf-doctor"],
    );
    must(&out, "runtimes: doctor on the slim package");
    let report = stdout_of(&out);
    assert!(
        report.contains("this runtime advertises: payload metadata version 1, selection-view"),
        "slim doctor omits the selection-view advertisement:\n{report}"
    );
    assert!(
        !report.contains("store-import"),
        "slim doctor advertises store-import:\n{report}"
    );
    assert!(
        report.contains("agreement: satisfied"),
        "slim doctor does not report agreement:\n{report}"
    );

    let out = run_pkg(
        &engine,
        &ubuntu,
        &td,
        "apps/cli-update/onelf-distro-update.onelf",
        &["--onelf-doctor"],
    );
    must(&out, "runtimes: doctor on the update package");
    assert!(
        stdout_of(&out).contains(
            "this runtime advertises: payload metadata version 1, selection-view+store-import"
        ),
        "update doctor omits the store-import advertisement:\n{}",
        transcript(&out)
    );

    // Both runtimes run on every pinned distro.
    for host in DISTROS {
        let host_tag = image(&engine, host);
        for rel in [
            "apps/cli-slim/onelf-distro-slim.onelf",
            "apps/cli-update/onelf-distro-update.onelf",
        ] {
            let out = run_pkg(&engine, &host_tag, &td, rel, &[]);
            must(&out, &format!("runtimes: {rel} on {host}"));
            assert!(
                stdout_of(&out).contains("CLI_OK ubuntu-24.04"),
                "runtimes: {rel} printed the wrong marker on {host}:\n{}",
                transcript(&out)
            );
        }
    }
}

/// The musl isolation guarantee: a glibc application packed with the
/// static musl runtime under a host-libs-never policy answers from its
/// bundled glibc on Ubuntu, on Fedora and on Alpine, so Alpine's musl is
/// never consulted; a musl application packed the same way answers musl
/// on a glibc host.
#[test]
fn a_glibc_app_packed_with_the_musl_runtime_stays_on_glibc_on_every_host() {
    let Some(engine) = gate() else { return };
    let ubuntu = image(&engine, "ubuntu-24.04");
    let td = Workdir::new("isolation");
    assemble(&engine, &ubuntu, &td, "ubuntu-24.04", &["libc"]);

    write_recipe(
        &td,
        "libc",
        &recipe(
            "onelf-distro-libc",
            "bin/libc",
            &["host-libs = \"never\""],
            &[],
        ),
    );
    must(
        &pack(&engine, &ubuntu, &td, "libc", false),
        "isolation: packing the glibc app",
    );
    let pkg = pkg_path(&td, "libc", "onelf-distro-libc");
    let rel = "apps/libc/onelf-distro-libc.onelf";

    // The package's own records state the policy that keeps the resolver
    // off the host entirely.
    assert_info_host_libs(&pkg, "never", "isolation: glibc package");
    let report = static_report(&engine, &ubuntu, &td, rel);
    assert_static_musl(&report, "isolation: glibc package runtime");

    // The bundled glibc answers with one version string on every host,
    // musl included. Whatever each host would load by default, the
    // package prints the same glibc it shipped with.
    let mut versions = Vec::new();
    for host in DISTROS {
        let host_tag = image(&engine, host);
        let out = run_pkg(&engine, &host_tag, &td, rel, &[]);
        must(&out, &format!("isolation: glibc package on {host}"));
        let text = stdout_of(&out);
        let version = text
            .lines()
            .find_map(|l| l.strip_prefix("libc: glibc "))
            .unwrap_or_else(|| {
                panic!(
                    "isolation: the glibc package did not print its glibc on {host}:\n{}",
                    transcript(&out)
                )
            })
            .trim()
            .to_string();
        versions.push((host, version));
    }
    let (_, bundled) = versions[0].clone();
    for (host, version) in versions.iter().skip(1) {
        assert_eq!(
            version, &bundled,
            "isolation: the glibc package printed a different libc on {host}"
        );
    }

    // The doctor report runs on the musl host too and names the
    // entrypoint the launch would take.
    let alpine = image(&engine, "alpine-3.20");
    let out = run_pkg(&engine, &alpine, &td, rel, &["--onelf-doctor"]);
    must(&out, "isolation: doctor on Alpine");
    let report = stdout_of(&out);
    assert!(
        report.contains("onelf-rt") && report.contains("entrypoint libc"),
        "isolation: the doctor report on Alpine is not about this package:\n{report}"
    );

    // The mirror direction: a musl application, packed the same way on
    // Alpine, answers musl on the glibc distro.
    let alpine_td = Workdir::new("isolation-musl");
    assemble(&engine, &alpine, &alpine_td, "alpine-3.20", &["libc"]);
    write_recipe(
        &alpine_td,
        "libc",
        &recipe(
            "onelf-distro-libc-musl",
            "bin/libc",
            &["host-libs = \"never\""],
            &[],
        ),
    );
    must(
        &pack(&engine, &alpine, &alpine_td, "libc", false),
        "isolation: packing the musl app",
    );
    let out = run_pkg(
        &engine,
        &ubuntu,
        &alpine_td,
        "apps/libc/onelf-distro-libc-musl.onelf",
        &[],
    );
    must(&out, "isolation: musl package on Ubuntu");
    assert!(
        stdout_of(&out).contains("libc: musl"),
        "isolation: the musl package did not answer musl on Ubuntu:\n{}",
        transcript(&out)
    );
}
