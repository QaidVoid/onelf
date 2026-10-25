mod bundle;
mod cache;
mod compress;
mod extract;
mod info;
mod init;
mod integrate;
mod list;
mod metadata;
mod pack;
mod payload;
mod recipe;
mod run;
mod sign;
mod sysroot_cmd;
mod verify;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use onelf_format::WorkingDir;

const RUNTIME_BINARY_SLIM: &[u8] = include_bytes!(env!("ONELF_RT_PATH"));
const RUNTIME_BINARY_UPDATE: &[u8] = include_bytes!(env!("ONELF_RT_UPDATE_PATH"));

#[derive(Parser)]
#[command(name = "onelf", about = "Single-binary packaging tool", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Pack a directory into a single executable
    Pack {
        /// Directory to pack
        directory: PathBuf,

        /// Output file path
        #[arg(short, long)]
        output: PathBuf,

        /// Relative path to the command within the directory
        #[arg(long)]
        command: String,

        /// Package name (for identification, defaults to command basename)
        #[arg(long)]
        name: Option<String>,

        /// Additional entrypoints (name=path)
        #[arg(long, value_parser = parse_entrypoint)]
        entrypoint: Vec<(String, String)>,

        /// Default entrypoint name
        #[arg(long)]
        default_entrypoint: Option<String>,

        /// Library directories of the bundle (repeatable).
        /// Pass "auto" (default) to detect directories containing .so files.
        #[arg(long, default_values_t = [String::from("auto")])]
        lib_dir: Vec<String>,

        /// Zstd compression level (0-22)
        #[arg(long, default_value = "19")]
        level: i32,

        /// Bytes per payload block, with an optional K/M suffix: 4K to 32M,
        /// 64K by default. A stipplefs payload takes a power of two up to
        /// 64M, 32M by default.
        #[arg(long, value_parser = parse_block_size)]
        block_size: Option<u64>,

        /// What the payload is made of: blocks compressed one by one, or a stipplefs
        /// image, which deduplicates and compresses across files
        #[arg(long, value_name = "FORMAT", default_value = "blocks")]
        payload: PayloadArg,

        /// Build a shared zstd dictionary
        #[arg(long)]
        dict: bool,

        /// Store the payload uncompressed (no zstd). Larger file, but
        /// zero decompression at runtime. Overrides --dict.
        #[arg(long)]
        no_compress: bool,

        /// Mark default entrypoint as memfd-eligible
        #[arg(long, conflicts_with = "no_memfd")]
        memfd: bool,

        /// Force cache mode (disable memfd)
        #[arg(long)]
        no_memfd: bool,

        /// Working directory strategy
        #[arg(long, default_value = "inherit")]
        working_dir: WorkingDirArg,

        /// HTTPS URL for delta updates (stored in .onelf/update-url)
        #[arg(long)]
        update_url: Option<String>,

        /// Path to a file holding one or more raw 32-byte Ed25519 public
        /// keys used to verify signed self-updates (stored in
        /// .onelf/update-key). Several concatenated keys form a keyring:
        /// the runtime accepts an update signed with any of them.
        #[arg(long, requires = "update_url")]
        update_key: Option<PathBuf>,

        /// Monotonic revision for rollback protection (stored in
        /// .onelf/update-revision); bump it for every release, the
        /// runtime refuses an update that is not newer
        #[arg(long, requires = "update_url")]
        update_revision: Option<u64>,

        /// Accept unsigned self-updates: store no signing key and mark
        /// the package (.onelf/update-unsigned) so its runtime installs
        /// whatever the update URL serves, warning on stderr. HTTPS is
        /// still required.
        #[arg(long, requires = "update_url", conflicts_with = "update_key")]
        update_unsigned: bool,

        /// What the launch resolver may take from the host: `never`
        /// nothing, `auto` the driver stack's closure, `always` any
        /// bundled soname. `auto` decides from the bundle's contents.
        #[arg(long, value_name = "MODE", default_value = "auto")]
        host_libs: HostLibsArg,

        /// Let the runtime fall back to the persistent cache when no
        /// other execution mode works. Off by default so the package
        /// leaves nothing on disk.
        #[arg(long)]
        cache: bool,

        /// Run from the persistent cache before trying any other execution
        /// mode, falling back to them when it cannot be extracted. For an
        /// app that records its own path in files that outlive it.
        /// Implies --cache.
        #[arg(long)]
        prefer_cache: bool,

        /// Record the update URL but do not embed the updater, linking the
        /// slim runtime instead. Saves about 1.4 MB per package. For
        /// packages updated by a package manager or deployment system,
        /// where the package must not replace itself.
        #[arg(long)]
        no_embed_updater: bool,

        /// Exclude files matching glob patterns (repeatable, e.g. "*.a", "__pycache__")
        #[arg(long)]
        exclude: Vec<String>,

        /// Library to dlopen on every exec via the onelf-env constructor
        /// (repeatable). Survives sandboxed re-exec, unlike LD_PRELOAD.
        #[arg(long)]
        preload: Vec<String>,

        /// The app runs setuid binaries such as sudo or pkexec, so keep it
        /// out of a user namespace, where a setuid bit does nothing.
        #[arg(long)]
        needs_setuid: bool,

        /// Pin every entry's mtime to this Unix timestamp for fully
        /// reproducible output, independent of filesystem timestamps.
        #[arg(long)]
        mtime: Option<u64>,

        /// Startup profile recorded with ONELF_PROFILE: the runtime
        /// decompresses the blocks it lists ahead of the app
        #[arg(long, value_name = "FILE")]
        prefetch: Option<PathBuf>,
    },

    /// Scaffold a starter onelf.toml
    Init {
        /// Path to write the recipe to
        #[arg(short, long, default_value = "onelf.toml")]
        output: PathBuf,

        /// Seed the recipe from a binary: sets name/command from its basename
        #[arg(long, value_name = "PATH")]
        binary: Option<PathBuf>,

        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
    },

    /// Build from an onelf.toml recipe (runs bundle-libs + pack)
    Build {
        /// Path to onelf.toml, or to the directory containing it (default: .)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Override the output path from the recipe
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Run an AppDir in place, without packing, for fast dev iteration
    Run {
        /// AppDir path, or a .toml recipe file (default: .)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Path to the binary to exec, relative to the AppDir (overrides any
        /// recipe-specified command). Handy when there's no onelf.toml.
        #[arg(long)]
        command: Option<String>,

        /// Entrypoint name to run (default: the recipe's default entrypoint)
        #[arg(long)]
        entrypoint: Option<String>,

        /// Run bundle-libs first using the recipe's [bundle] settings. Useful
        /// for a one-shot dev loop; does nothing if the AppDir has no recipe.
        #[arg(long)]
        bundle: bool,

        /// Arguments passed to the entrypoint after its own args
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Verify a packed binary's integrity (recompute hashes vs manifest)
    Verify {
        /// Path to the onelf binary
        binary: PathBuf,
        /// Ed25519 public key, raw 32 bytes as `key new` writes it, or a
        /// keyring of several concatenated keys; with --sig, also check a
        /// detached signature over the whole file against any of them
        #[arg(long)]
        key: Option<PathBuf>,
        /// Detached signature over the whole binary, raw 64 bytes as
        /// `sign` writes them; requires --key
        #[arg(long)]
        sig: Option<PathBuf>,
    },

    /// Obtain and inspect pinned sysroots
    Sysroot {
        #[command(subcommand)]
        action: SysrootAction,
    },

    /// Show metadata about a packed binary
    Info {
        /// Path to the onelf binary
        binary: PathBuf,
    },

    /// List all files in a packed binary
    List {
        /// Path to the onelf binary
        binary: PathBuf,
    },

    /// Extract files from a packed binary
    Extract {
        /// Path to the onelf binary
        binary: PathBuf,

        /// Output path (directory, file, or "-" for stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Extract only specific files by path (repeatable)
        #[arg(long)]
        file: Vec<String>,

        /// Preserve full mode bits including setuid/setgid/sticky.
        /// Unsafe for untrusted packages; off by default (masked to 0o777).
        #[arg(long)]
        preserve_mode: bool,
    },

    /// Extract icon from a packed binary
    Icon {
        /// Path to the onelf binary
        binary: PathBuf,

        /// Entrypoint name (default: default entrypoint)
        #[arg(long)]
        entrypoint: Option<String>,

        /// Output path (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Extract desktop file from a packed binary
    Desktop {
        /// Path to the onelf binary
        binary: PathBuf,

        /// Entrypoint name (default: default entrypoint)
        #[arg(long)]
        entrypoint: Option<String>,

        /// Output path (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Install desktop shortcut and icon for a packed binary (XDG integration)
    Integrate {
        /// Path to the onelf binary
        binary: PathBuf,

        /// Entrypoint name (default: the one BINARY launches, which for a
        /// symlink is the entrypoint its name selects)
        #[arg(long)]
        entrypoint: Option<String>,
    },

    /// Remove desktop shortcut and icon installed by integrate
    Unintegrate {
        /// Path to the onelf binary
        binary: PathBuf,

        /// Entrypoint name (default: the one BINARY launches, which for a
        /// symlink is the entrypoint its name selects)
        #[arg(long)]
        entrypoint: Option<String>,
    },

    /// Manage signing keys for self-update
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },

    /// Sign a package so its self-update can be verified
    ///
    /// Writes the detached Ed25519 signature the runtime fetches from the
    /// update URL with `.sig` appended. Sign after every step that changes
    /// the packaged bytes, or the signature covers a stale revision.
    Sign {
        /// Path to the packaged binary to sign
        binary: PathBuf,

        /// Secret key file from `onelf key new` (64 raw bytes)
        #[arg(long, value_name = "PATH")]
        key: PathBuf,

        /// Where to write the signature (default: the update URL's file
        /// name with `.sig` appended, next to BINARY, or <BINARY>.sig when
        /// the package has no update URL)
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,
    },

    /// Manage the onelf cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

    /// Bundle shared library dependencies into a directory
    BundleLibs {
        /// Directory containing the application to bundle. When used with
        /// --from-binary, this is the output directory (created if needed).
        directory: PathBuf,

        /// Specific binary to analyze (default: scan all ELF files)
        #[arg(long)]
        target: Option<PathBuf>,

        /// Scaffold a fresh AppDir: copy this binary into <DIRECTORY>/bin/
        /// (creating the directory) before bundling its dependencies.
        #[arg(long, value_name = "PATH")]
        from_binary: Option<PathBuf>,

        /// Where to copy libs, relative to DIRECTORY
        #[arg(long, default_value = "lib")]
        lib_dir: PathBuf,

        /// Exclude libs matching pattern (prefix match, comma-separated or repeatable)
        #[arg(long, value_delimiter = ',')]
        exclude: Vec<String>,

        /// Additional libraries to include by soname (e.g. dlopen'd libs, comma-separated or repeatable)
        #[arg(long, value_delimiter = ',')]
        include: Vec<String>,

        /// Additional directories to search for libraries (repeatable)
        #[arg(long)]
        search_path: Vec<PathBuf>,

        /// Show what would be copied without copying
        #[arg(long)]
        dry_run: bool,

        /// Don't resolve transitive dependencies
        #[arg(long)]
        no_recursive: bool,

        /// Bundle Mesa GL/EGL/GBM libraries
        #[arg(long, conflicts_with = "sysroot")]
        gl: bool,

        /// Bundle DRI drivers (architecture-filtered)
        #[arg(long, conflicts_with = "sysroot")]
        dri: bool,

        /// Bundle Vulkan ICD drivers (architecture-filtered)
        #[arg(long, conflicts_with = "sysroot")]
        vulkan: bool,

        /// Bundle Wayland client libraries (libwayland, libdecor, libxkbcommon)
        #[arg(long, conflicts_with = "sysroot")]
        wayland: bool,

        /// Bundle GSettings schemas for GTK apps
        #[arg(long, conflicts_with = "sysroot")]
        gtk: bool,

        /// Do not bundle Mesa GL/EGL/GBM libraries (overrides --gl)
        #[arg(long)]
        no_gl: bool,

        /// Do not bundle DRI drivers (overrides --dri)
        #[arg(long)]
        no_dri: bool,

        /// Do not bundle Vulkan ICD drivers (overrides --vulkan)
        #[arg(long)]
        no_vulkan: bool,

        /// Do not bundle Wayland client libraries even if auto-detected
        #[arg(long)]
        no_wayland: bool,

        /// Do not bundle GSettings schemas even if auto-detected
        #[arg(long)]
        no_gtk: bool,

        /// Strip debug symbols from copied libraries
        #[arg(long)]
        strip: bool,

        /// Skip libraries whose libc family (musl vs glibc) doesn't match
        /// the target binary. Without this flag, mismatched libs are copied
        /// with a warning.
        #[arg(long)]
        strict_libc: bool,

        /// Scan binary strings for soname-shaped values that match a known
        /// allow-list of commonly dlopen'd libraries (GL, Wayland, Vulkan,
        /// audio, DBus, etc.) and bundle the matches.
        #[arg(long)]
        scan_dlopen: bool,

        /// Extra sonames added to the --scan-dlopen allow-list (repeatable
        /// or comma-separated). Matches must still appear in the binary's
        /// strings to be bundled.
        #[arg(long, value_delimiter = ',')]
        dlopen: Vec<String>,

        /// Run the target for a few seconds and bundle every library it
        /// loaded at runtime: what dlopen by computed name hides from
        /// DT_NEEDED. Needs --target. Uses the glibc loader's own report;
        /// a musl target needs strace on PATH.
        #[arg(long, requires = "target")]
        trace_run: bool,

        /// How long the traced run may go before it is stopped
        #[arg(long, default_value = "5", requires = "trace_run")]
        trace_seconds: u64,

        /// Argument for the traced run (repeatable; `--trace-arg --version`
        /// passes `--version` through)
        #[arg(long, requires = "trace_run", allow_hyphen_values = true)]
        trace_arg: Vec<String>,

        /// Take the bundle's contents from this materialized sysroot's
        /// package database instead of scanning this machine. Needs
        /// --target to name the entrypoint.
        #[arg(long, value_name = "DIR")]
        sysroot: Option<PathBuf>,

        /// Optional dependency to include from the sysroot (repeatable)
        #[arg(long, value_name = "PACKAGE", requires = "sysroot")]
        sysroot_optional: Vec<String>,

        /// File of soname prefixes the host provides, one per line
        #[arg(long, value_name = "FILE", requires = "sysroot")]
        platform_line: Option<PathBuf>,

        /// File of glob patterns that never ship, one per line
        #[arg(long, value_name = "FILE", requires = "sysroot")]
        policy: Option<PathBuf>,

        /// File of paths a test run opened, one per line
        #[arg(long, value_name = "FILE", requires = "sysroot")]
        trace: Option<PathBuf>,

        /// File of glob patterns the trace may not prune
        #[arg(long, value_name = "FILE", requires = "sysroot")]
        keep: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum KeyAction {
    /// Generate a keypair for signing self-updates
    ///
    /// The public key is written as the 32 raw bytes `pack --update-key`
    /// embeds, so it needs no conversion. The secret key is owner-only.
    New {
        /// Where to write the secret key
        #[arg(long, value_name = "PATH", default_value = "onelf.key")]
        secret: PathBuf,

        /// Where to write the public key
        #[arg(long, value_name = "PATH", default_value = "onelf.pub")]
        public: PathBuf,
    },

    /// Write the public key belonging to an existing secret key
    Show {
        /// Secret key file to derive from
        #[arg(long, value_name = "PATH")]
        secret: PathBuf,

        /// Where to write the public key (default: stdout)
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// List cached packages
    List,
    /// Remove all cached data
    Clear,
    /// Garbage collect old cache entries
    Gc {
        /// Maximum age in days
        #[arg(long, default_value = "30")]
        max_age: u64,
    },
}

#[derive(Subcommand)]
enum SysrootAction {
    /// Materialize a rootfs archive, from a local path or an https:// URL
    Fetch {
        /// Archive path or URL (.tar or .tar.zst)
        source: String,
        /// Directory to materialize into
        dir: PathBuf,
        /// BLAKE3 hash the archive must have before it is unpacked
        #[arg(long)]
        hash: Option<String>,
    },
    /// Show what a materialized sysroot holds
    Info {
        /// A materialized sysroot
        dir: PathBuf,
    },
    /// Pack a GL build for hosts without one, and print the hash to pin
    PackGl {
        /// A tree holding lib/, share/vulkan/icd.d and friends; built
        /// here from the sysroot when --package is given
        dir: PathBuf,
        /// Output file
        #[arg(short, long)]
        output: PathBuf,
        /// A materialized sysroot to build the tree from
        #[arg(long, value_name = "DIR", requires = "package")]
        sysroot: Option<PathBuf>,
        /// Packages whose closure the build holds (mesa, vulkan-radeon, ...)
        #[arg(long, value_name = "NAME", requires = "sysroot")]
        package: Vec<String>,
    },
    /// Pack a shared dependency set for packages to fetch instead of
    /// bundling it, and print the hash to pin
    PackSet {
        /// A tree holding the set; built here from the sysroot when
        /// --package is given
        dir: PathBuf,
        /// Output file
        #[arg(short, long)]
        output: PathBuf,
        /// The set's name, as the recipe refers to it
        #[arg(long)]
        name: String,
        /// A materialized sysroot to build the tree from
        #[arg(long)]
        sysroot: Option<PathBuf>,
        /// Packages whose closure the set holds (qt6-base, gtk4, ...)
        #[arg(long, requires = "sysroot")]
        package: Vec<String>,
        /// File of soname prefixes the host provides, one per line.
        /// Without one the GPU driver families are the line, so the
        /// stack behind them stays with the host.
        #[arg(long, requires = "sysroot")]
        platform_line: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum HostLibsArg {
    /// Decide from the bundle's contents
    Auto,
    /// Compare every bundled soname against the host
    Always,
    /// Take nothing from the host
    Never,
}

impl From<HostLibsArg> for pack::HostLibs {
    fn from(v: HostLibsArg) -> Self {
        match v {
            HostLibsArg::Auto => pack::HostLibs::Auto,
            HostLibsArg::Always => pack::HostLibs::Always,
            HostLibsArg::Never => pack::HostLibs::Never,
        }
    }
}

#[derive(Clone, ValueEnum)]
enum PayloadArg {
    /// Each file cut into blocks that are compressed one by one
    Blocks,
    /// A stipplefs image
    Stipple,
}

impl From<PayloadArg> for pack::Payload {
    fn from(v: PayloadArg) -> Self {
        match v {
            PayloadArg::Blocks => pack::Payload::Blocks,
            PayloadArg::Stipple => pack::Payload::Stipple,
        }
    }
}

#[derive(Clone, ValueEnum)]
enum WorkingDirArg {
    Inherit,
    Package,
    Command,
}

fn parse_entrypoint(s: &str) -> Result<(String, String), String> {
    let (name, path) = s.split_once('=').ok_or("expected format: name=path")?;
    Ok((name.to_string(), path.to_string()))
}

/// Parse a block size in bytes, accepting a K or M suffix.
fn parse_block_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let (digits, scale) = match t.as_bytes().last() {
        Some(b'K' | b'k') => (&t[..t.len() - 1], 1024),
        Some(b'M' | b'm') => (&t[..t.len() - 1], 1024 * 1024),
        _ => (t, 1),
    };
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid block size '{s}': expected a number, optionally K or M"))?;
    let bytes = n
        .checked_mul(scale)
        .ok_or_else(|| format!("block size '{s}' overflows"))?;

    let (min, max) = (compress::MIN_BLOCK_SIZE, compress::MAX_STIPPLE_BLOCK_SIZE);
    if !(min..=max).contains(&bytes) {
        return Err(format!(
            "block size {bytes} out of range: expected {}K to {}M",
            min / 1024,
            max / (1024 * 1024)
        ));
    }
    Ok(bytes)
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Pack {
            directory,
            output,
            command,
            name,
            entrypoint,
            default_entrypoint,
            lib_dir,
            level,
            block_size,
            payload,
            dict,
            no_compress,
            memfd,
            no_memfd,
            working_dir,
            update_url,
            update_key,
            update_revision,
            update_unsigned,
            no_embed_updater,
            host_libs,
            cache,
            prefer_cache,
            exclude,
            preload,
            needs_setuid,
            mtime,
            prefetch,
        } => {
            let payload: pack::Payload = payload.into();
            let memfd_opt = if no_memfd {
                Some(false)
            } else if memfd {
                Some(true)
            } else {
                None
            };

            let wd = match working_dir {
                WorkingDirArg::Inherit => WorkingDir::Inherit,
                WorkingDirArg::Package => WorkingDir::PackageRoot,
                WorkingDirArg::Command => WorkingDir::EntrypointParent,
            };
            let pins_https = pins_https_build(&directory);

            let entrypoints = entrypoint
                .into_iter()
                .map(|(n, p)| (n, p, Vec::new()))
                .collect();
            if no_embed_updater && update_url.is_none() {
                eprintln!(
                    "warning: --no-embed-updater has no effect without --update-url; \
                     the slim runtime is already used when no update URL is set"
                );
            }
            let update_key = update_key.as_deref().map(std::fs::read).transpose();
            let profile = prefetch.as_deref().map(pack::read_profile).transpose();
            match update_key.and_then(|key| Ok((key, profile?.unwrap_or_default()))) {
                Err(e) => Err(e),
                Ok((update_key, profile)) => pack::pack(
                    &pack::PackOptions {
                        directory,
                        output,
                        command,
                        name,
                        entrypoints,
                        default_entrypoint,
                        lib_dirs: lib_dir,
                        level,
                        block_size: block_size.unwrap_or(payload.default_block_size()),
                        payload,
                        use_dict: dict,
                        no_compress,
                        memfd: memfd_opt,
                        working_dir: wd,
                        host_libs: host_libs.into(),
                        cache,
                        prefer_cache,
                        update_url: update_url.clone(),
                        embed_updater: !no_embed_updater,
                        update_key,
                        update_revision,
                        update_unsigned,
                        exclude,
                        package_info: None,
                        mtime,
                        env: Vec::new(),
                        paths: Vec::new(),
                        preload,
                        needs_setuid,
                        profile,
                    },
                    // Pick the runtime: slim (~700KB) by default; the
                    // update-capable runtime (~2MB) when the user configures
                    // self-updates or the package pins a GL build over HTTPS.
                    if (update_url.is_some() && !no_embed_updater) || pins_https {
                        RUNTIME_BINARY_UPDATE
                    } else {
                        RUNTIME_BINARY_SLIM
                    },
                ),
            }
        }
        Commands::Init {
            output,
            binary,
            force,
        } => init::init(&output, binary.as_deref(), force),
        Commands::Build { path, output } => run_build(&path, output.as_deref()),
        Commands::Run {
            path,
            command,
            entrypoint,
            bundle,
            args,
        } => run::run(
            &path,
            command.as_deref(),
            entrypoint.as_deref(),
            bundle,
            &args,
        ),
        Commands::Verify { binary, key, sig } => {
            verify::verify(&binary, key.as_deref(), sig.as_deref())
        }
        Commands::Info { binary } => info::info(&binary),
        Commands::List { binary } => list::list(&binary),
        Commands::Extract {
            binary,
            output,
            file,
            preserve_mode,
        } => extract::extract(&binary, output.as_deref(), &file, preserve_mode),
        Commands::Icon {
            binary,
            entrypoint,
            output,
        } => metadata::icon(&binary, entrypoint.as_deref(), output.as_deref()),
        Commands::Desktop {
            binary,
            entrypoint,
            output,
        } => metadata::desktop(&binary, entrypoint.as_deref(), output.as_deref()),
        Commands::Integrate { binary, entrypoint } => {
            integrate::integrate(&binary, entrypoint.as_deref())
        }
        Commands::Unintegrate { binary, entrypoint } => {
            integrate::unintegrate(&binary, entrypoint.as_deref())
        }
        Commands::Key { action } => match action {
            KeyAction::New { secret, public } => sign::generate(&public, &secret).map(|()| {
                println!("Secret key: {} (keep private)", secret.display());
                println!("Public key: {}", public.display());
                println!(
                    "Pack with: onelf pack ... --update-key {}",
                    public.display()
                );
            }),
            KeyAction::Show { secret, output } => {
                sign::public_key_of(&secret).and_then(|pk| match output {
                    Some(path) => std::fs::write(&path, pk),
                    None => std::io::Write::write_all(&mut std::io::stdout(), &pk),
                })
            }
        },
        Commands::Sign {
            binary,
            key,
            output,
        } => sign::sign(&binary, &key, output.as_deref()).map(|signed| {
            match signed.matched {
                sign::KeyMatch::Matches => {
                    println!("Signed with the key this package embeds.");
                }
                sign::KeyMatch::NoEmbeddedKey => {
                    eprintln!(
                        "warning: {} embeds no update key, so it cannot self-update. \
                         Repack with --update-key to make this signature useful.",
                        binary.display()
                    );
                }
                sign::KeyMatch::NotAPackage => {
                    eprintln!(
                        "warning: {} is not an onelf package, so the signing key could \
                         not be checked against it.",
                        binary.display()
                    );
                }
            }
            println!("Signature: {}", signed.path.display());
            if let Some(url) = signed.sig_url {
                println!("Publish it at: {url}");
            }
        }),
        Commands::Cache { action } => match action {
            CacheAction::List => cache::cache_list(),
            CacheAction::Clear => cache::cache_clear(),
            CacheAction::Gc { max_age } => cache::cache_gc(max_age),
        },
        Commands::BundleLibs {
            directory,
            target,
            from_binary,
            lib_dir,
            exclude,
            include,
            search_path,
            dry_run,
            no_recursive,
            gl,
            dri,
            vulkan,
            wayland,
            gtk,
            no_gl,
            no_dri,
            no_vulkan,
            no_wayland,
            no_gtk,
            strip,
            strict_libc,
            scan_dlopen,
            dlopen,
            trace_run,
            trace_seconds,
            trace_arg,
            sysroot,
            sysroot_optional,
            platform_line,
            policy,
            trace,
            keep,
        } => scaffold_from_binary(&directory, from_binary.as_deref()).and_then(|_| {
            let sysroot = match sysroot {
                Some(root) => {
                    let Some(command) = target.as_ref().and_then(|t| t.to_str()) else {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "--sysroot needs --target to name the entrypoint",
                        ));
                    };
                    Some(bundle::SysrootOptions {
                        platform: bundle::sysroot::default_label(&root, None),
                        root,
                        command: command.to_string(),
                        optional: sysroot_optional,
                        platform_line,
                        policy,
                        trace,
                        keep,
                        platform_url: None,
                        platform_hash: None,
                        sets: Vec::new(),
                    })
                }
                None => None,
            };
            bundle::bundle_libs(&bundle::BundleOptions {
                directory,
                primary: target.clone(),
                target,
                lib_dir,
                exclude,
                include,
                search_path,
                dry_run,
                recursive: !no_recursive,
                gl,
                dri,
                vulkan,
                wayland,
                gtk,
                no_gl,
                no_dri,
                no_vulkan,
                no_wayland,
                no_gtk,
                strip,
                strict_libc,
                scan_dlopen,
                dlopen_extra: dlopen,
                trace_run,
                trace_seconds,
                trace_args: trace_arg,
                sysroot,
            })
        }),
        Commands::Sysroot { action } => match action {
            SysrootAction::Fetch { source, dir, hash } => {
                sysroot_cmd::fetch(&source, &dir, hash.as_deref())
            }
            SysrootAction::Info { dir } => sysroot_cmd::info(&dir),
            SysrootAction::PackGl {
                dir,
                output,
                sysroot,
                package,
            } => {
                let built = match &sysroot {
                    Some(root) => std::fs::create_dir_all(&dir)
                        .and_then(|()| bundle::sysroot::populate_gl(&dir, root, &package))
                        .map(Some),
                    None => Ok(None),
                };
                built.and_then(|selection| {
                    if let Some(selection) = &selection {
                        let named: Vec<String> = selection
                            .packages
                            .iter()
                            .filter(|(n, _)| package.contains(n))
                            .map(|(n, v)| format!("{n} {v}"))
                            .collect();
                        eprintln!(
                            "Built from {}: {} files from {} packages ({} and their dependencies)",
                            sysroot
                                .as_deref()
                                .unwrap_or(std::path::Path::new("?"))
                                .display(),
                            selection.copied,
                            selection.packages.len(),
                            named.join(", ")
                        );
                    }
                    sysroot_cmd::pack_gl(&dir, &output, sysroot.as_deref(), RUNTIME_BINARY_SLIM)
                })
            }
            SysrootAction::PackSet {
                dir,
                output,
                name,
                sysroot,
                package,
                platform_line,
            } => {
                let line =
                    bundle::sysroot::check_set_name(&name).and_then(|()| match &platform_line {
                        Some(p) => onelf_sysroot::PlatformLine::load(p),
                        None => Ok(onelf_sysroot::PlatformLine::from_prefixes(
                            onelf_format::drivers::DRIVER_FAMILIES
                                .iter()
                                .map(|s| s.to_string())
                                .collect(),
                        )),
                    });
                let built = line.and_then(|line| match &sysroot {
                    Some(root) => std::fs::create_dir_all(&dir)
                        .and_then(|()| bundle::sysroot::populate_set(&dir, root, &package, &line))
                        .map(Some),
                    None => Ok(None),
                });
                built.and_then(|selection| {
                    if let Some(selection) = &selection {
                        eprintln!(
                            "Built set {name} from {}: {} files from {} packages",
                            sysroot
                                .as_deref()
                                .unwrap_or(std::path::Path::new("?"))
                                .display(),
                            selection.copied,
                            selection.packages.len(),
                        );
                    }
                    sysroot_cmd::pack_set(
                        &dir,
                        &output,
                        &name,
                        sysroot.as_deref(),
                        RUNTIME_BINARY_SLIM,
                    )
                })
            }
        },
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// The sysroot a recipe names, materialized from its archive when the
/// directory does not exist yet. Paths resolve against the recipe's
/// directory.
fn sysroot_from_recipe(
    dir: &std::path::Path,
    command: &str,
    sr: &recipe::Sysroot,
) -> std::io::Result<bundle::SysrootOptions> {
    let resolve = |p: &std::path::Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            dir.join(p)
        }
    };
    let root = resolve(&sr.path);
    bundle::sysroot::refuse_inside(&root, dir)?;
    if !root.is_dir() {
        let Some(archive) = &sr.archive else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "{}: sysroot does not exist and [sysroot] names no archive",
                    root.display()
                ),
            ));
        };
        // A URL is handed over as written; only a path is relative to the
        // recipe.
        let archive_str = archive.to_string_lossy();
        let source = if archive_str.contains("://") {
            archive_str.into_owned()
        } else {
            resolve(archive).to_string_lossy().into_owned()
        };
        sysroot_cmd::fetch(&source, &root, sr.hash.as_deref())?;
    }
    let platform = sr
        .platform
        .clone()
        .unwrap_or_else(|| bundle::sysroot::default_label(&root, sr.archive.as_deref()));
    Ok(bundle::SysrootOptions {
        root,
        command: command.to_string(),
        platform,
        optional: sr.optional.clone(),
        platform_line: sr.platform_line.as_deref().map(resolve),
        policy: sr.policy.as_deref().map(resolve),
        trace: sr.trace.as_deref().map(resolve),
        keep: sr.keep.as_deref().map(resolve),
        platform_url: sr.platform_url.clone(),
        platform_hash: sr.platform_hash.clone(),
        sets: sr
            .sets
            .iter()
            .map(|(name, set)| bundle::sysroot::SharedSet {
                name: name.clone(),
                packages: set.packages.clone(),
                url: set.url.clone(),
                blake3: set.blake3.clone(),
            })
            .collect(),
    })
}

/// Whether the AppDir pins a GL build over HTTPS, which only the
/// update-capable runtime can fetch.
fn pins_https_build(dir: &std::path::Path) -> bool {
    [bundle::sysroot::PLATFORM_FILE, bundle::sysroot::SETS_FILE]
        .iter()
        .any(|f| {
            std::fs::read_to_string(dir.join(f))
                .map(|t| t.lines().any(|l| l.starts_with("url = \"https://")))
                .unwrap_or(false)
        })
}

fn run_build(
    path: &std::path::Path,
    output_override: Option<&std::path::Path>,
) -> std::io::Result<()> {
    let recipe_path = recipe::resolve(path)?;
    let recipe = recipe::load(&recipe_path)?;
    let dir = recipe_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // A sysroot build does not use the framework bundlers, so their flags
    // would do nothing. `--sysroot` refuses them, and so does a recipe.
    let b = &recipe.bundle;
    let frameworks: Vec<&str> = [
        ("gl", b.gl),
        ("dri", b.dri),
        ("vulkan", b.vulkan),
        ("wayland", b.wayland),
        ("gtk", b.gtk),
    ]
    .into_iter()
    .filter_map(|(name, on)| on.then_some(name))
    .collect();
    if recipe.sysroot.is_some() && !frameworks.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{}: [bundle] {} cannot be used with [sysroot]. A sysroot \
                 build takes the bundle's contents from the sysroot and does \
                 not use the framework bundlers",
                recipe_path.display(),
                frameworks.join(", ")
            ),
        ));
    }

    let sysroot = match &recipe.sysroot {
        Some(sr) => Some(sysroot_from_recipe(&dir, &recipe.package.command, sr)?),
        None => None,
    };

    // A sysroot build is reproducible from its archive and recipe alone,
    // so the time the AppDir was written does not reach the package
    // unless the recipe or SOURCE_DATE_EPOCH asks for a time.
    let mtime = recipe
        .package
        .mtime
        .or((recipe.sysroot.is_some() && pack::source_date_epoch().is_none()).then_some(0));

    // Stage 1: bundle-libs. A sysroot is applied by bundle-libs, so it
    // runs even when the recipe would otherwise skip bundling.
    if sysroot.is_some() || !recipe.bundle.skip {
        let search_path: Vec<PathBuf> = recipe
            .bundle
            .search_paths
            .iter()
            .map(|s| PathBuf::from(s.as_str()))
            .collect();

        bundle::bundle_libs(&bundle::BundleOptions {
            directory: dir.clone(),
            target: None,
            primary: Some(PathBuf::from(&recipe.package.command)),
            lib_dir: PathBuf::from("lib"),
            exclude: recipe.bundle.exclude.clone(),
            include: recipe.bundle.include.clone(),
            search_path,
            dry_run: false,
            recursive: true,
            gl: recipe.bundle.gl,
            dri: recipe.bundle.dri,
            vulkan: recipe.bundle.vulkan,
            wayland: recipe.bundle.wayland,
            gtk: recipe.bundle.gtk,
            no_gl: recipe.bundle.no_gl,
            no_dri: recipe.bundle.no_dri,
            no_vulkan: recipe.bundle.no_vulkan,
            no_wayland: recipe.bundle.no_wayland,
            no_gtk: recipe.bundle.no_gtk,
            strip: recipe.bundle.strip,
            strict_libc: recipe.bundle.strict_libc,
            scan_dlopen: recipe.bundle.scan_dlopen,
            dlopen_extra: recipe.bundle.dlopen.clone(),
            trace_run: recipe.bundle.trace_run,
            trace_seconds: recipe.bundle.trace_seconds.unwrap_or(5),
            trace_args: recipe.bundle.trace_args.clone(),
            sysroot,
        })?;
    }

    // Stage 2: pack. Relative output paths in the recipe are resolved
    // against the recipe's directory.
    let output = match output_override {
        Some(o) => o.to_path_buf(),
        None => match recipe.package.output.clone() {
            Some(o) if o.is_absolute() => o,
            Some(o) => dir.join(o),
            None => {
                let name = recipe.package.name.clone().unwrap_or_else(|| {
                    recipe
                        .package
                        .command
                        .rsplit('/')
                        .next()
                        .unwrap_or("app")
                        .to_string()
                });
                dir.join(format!("{name}.onelf"))
            }
        },
    };

    let entrypoints: Vec<(String, String, Vec<String>)> = recipe
        .entrypoint
        .iter()
        .map(|e| (e.name.clone(), e.path.clone(), e.args.clone()))
        .collect();
    let default_entrypoint = recipe
        .entrypoint
        .iter()
        .find(|e| e.default)
        .map(|e| e.name.clone());

    let lib_dirs = recipe
        .bundle
        .lib_dirs
        .clone()
        .unwrap_or_else(|| vec!["auto".to_string()]);

    let update_url = recipe.update.as_ref().map(|u| u.url.clone());
    let update_key = recipe
        .update
        .as_ref()
        .and_then(|u| u.key.clone())
        .map(|k| if k.is_absolute() { k } else { dir.join(k) })
        .map(std::fs::read)
        .transpose()?;
    let update_revision = recipe.update.as_ref().and_then(|u| u.revision);
    let update_unsigned = recipe.update.as_ref().map(|u| u.unsigned).unwrap_or(false);
    let embed_updater = recipe.update.as_ref().and_then(|u| u.embed).unwrap_or(true);
    let runtime: &[u8] = if (update_url.is_some() && embed_updater) || pins_https_build(&dir) {
        RUNTIME_BINARY_UPDATE
    } else {
        RUNTIME_BINARY_SLIM
    };

    let package_info = build_package_info(&recipe.package);
    let profile = match &recipe.compression.prefetch {
        Some(path) => pack::read_profile(&dir.join(path))?,
        None => Vec::new(),
    };

    let payload: pack::Payload = recipe.compression.payload.into();
    pack::pack(
        &pack::PackOptions {
            directory: dir,
            output,
            command: recipe.package.command,
            name: recipe.package.name,
            entrypoints,
            default_entrypoint,
            lib_dirs,
            level: recipe.compression.level,
            block_size: recipe
                .compression
                .block_size
                .unwrap_or(payload.default_block_size()),
            payload,
            use_dict: recipe.compression.dict,
            no_compress: recipe.compression.store,
            memfd: recipe.package.memfd,
            working_dir: recipe.package.working_dir.into(),
            host_libs: recipe
                .package
                .host_libs
                .map(Into::into)
                .unwrap_or(pack::HostLibs::Auto),
            cache: recipe.package.cache,
            prefer_cache: recipe.package.prefer_cache,
            update_url,
            embed_updater,
            update_key,
            update_revision,
            update_unsigned,
            exclude: recipe.package.exclude,
            package_info,
            mtime,
            env: recipe.env.into_iter().collect(),
            paths: recipe.paths.into_iter().collect(),
            preload: recipe.preload,
            needs_setuid: recipe.package.needs_setuid,
            profile,
        },
        runtime,
    )
}

/// Serialize optional package metadata fields to TOML. Returns None if no
/// metadata is present.
fn build_package_info(pkg: &recipe::Package) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(ref s) = pkg.name {
        lines.push(format!("name = {}", toml_str(s)));
    }
    if let Some(ref s) = pkg.version {
        lines.push(format!("version = {}", toml_str(s)));
    }
    if let Some(ref s) = pkg.description {
        lines.push(format!("description = {}", toml_str(s)));
    }
    if let Some(ref s) = pkg.license {
        lines.push(format!("license = {}", toml_str(s)));
    }
    if let Some(ref s) = pkg.homepage {
        lines.push(format!("homepage = {}", toml_str(s)));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n") + "\n")
    }
}

/// Quote `s` as a TOML basic string, escaping backslash, double quote, and
/// control characters (per the TOML spec) so a value containing a newline or
/// other control char cannot produce a malformed `.onelf/package-info.toml`.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// If `src` is given, copy it into `dir/bin/<basename>`, creating `dir/bin`
/// first. No-op when `src` is None.
fn scaffold_from_binary(
    dir: &std::path::Path,
    src: Option<&std::path::Path>,
) -> std::io::Result<()> {
    let Some(src) = src else {
        return Ok(());
    };
    if !src.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("--from-binary: {} is not a file", src.display()),
        ));
    }
    let bin_dir = dir.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let name = src.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--from-binary has no filename",
        )
    })?;
    let dest = bin_dir.join(name);
    std::fs::copy(src, &dest)?;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    eprintln!("Scaffolded {} -> {}", src.display(), dest.display());
    Ok(())
}

#[cfg(test)]
mod block_size_tests {
    use super::parse_block_size;
    use crate::compress::{MAX_STIPPLE_BLOCK_SIZE, MIN_BLOCK_SIZE};

    #[test]
    fn accepts_plain_bytes_and_k_m_suffixes() {
        assert_eq!(parse_block_size("262144").unwrap(), 256 * 1024);
        assert_eq!(parse_block_size("256K").unwrap(), 256 * 1024);
        assert_eq!(parse_block_size("256k").unwrap(), 256 * 1024);
        assert_eq!(parse_block_size("4M").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_block_size(" 1M ").unwrap(), 1024 * 1024);
    }

    #[test]
    fn rejects_sizes_outside_the_supported_range() {
        assert!(parse_block_size(&(MIN_BLOCK_SIZE - 1).to_string()).is_err());
        assert!(parse_block_size(&(MAX_STIPPLE_BLOCK_SIZE + 1).to_string()).is_err());
        assert!(parse_block_size("0").is_err());
    }

    #[test]
    fn rejects_garbage_rather_than_silently_defaulting() {
        assert!(parse_block_size("").is_err());
        assert!(parse_block_size("M").is_err());
        assert!(parse_block_size("4G").is_err());
        assert!(parse_block_size("-1M").is_err());
        // Must error, not wrap to a small block.
        assert!(parse_block_size("18014398509481985M").is_err());
    }
}
