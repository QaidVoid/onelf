mod assembly;
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
        /// slim runtime instead. Saves about 1.3 MB per package. For
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
        /// no name can be derived from the update URL)
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
            let pins_https = assembly::pins_https_build(&directory);

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
            let update_key = update_key
                .as_deref()
                .map(|path| {
                    std::fs::read(path).map_err(|e| {
                        std::io::Error::new(
                            e.kind(),
                            format!("--update-key {}: {e}", path.display()),
                        )
                    })
                })
                .transpose();
            let profile = prefetch.as_deref().map(pack::read_profile).transpose();
            let runtime: &[u8] = if (update_url.is_some() && !no_embed_updater) || pins_https {
                assembly::RUNTIME_BINARY_UPDATE
            } else {
                assembly::RUNTIME_BINARY_SLIM
            };
            match update_key.and_then(|key| Ok((key, profile?.unwrap_or_default()))) {
                Err(e) => Err(e),
                Ok((update_key, profile)) => {
                    match onelf_format::elf::probe_capability_note(runtime) {
                        Err(e) => Err(e.into()),
                        Ok(runtime_note) => pack::require_store_runtime(&runtime_note, pins_https)
                            .and_then(|()| {
                                pack::pack(
                                    &pack::PackOptions {
                                        directory,
                                        output,
                                        command,
                                        name,
                                        entrypoints,
                                        default_entrypoint,
                                        lib_dirs: lib_dir,
                                        level,
                                        block_size: block_size
                                            .unwrap_or(payload.default_block_size()),
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
                                        provider_index: None,
                                        metadata: pack::MetadataPayload::default(),
                                        runtime_note,
                                    },
                                    runtime,
                                )
                            }),
                    }
                }
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
                sets: Vec::new(),
                entrypoint_selections: Vec::new(),
                platform_pin: None,
                strict: false,
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
                    sysroot_cmd::pack_gl(
                        &dir,
                        &output,
                        sysroot.as_deref(),
                        assembly::RUNTIME_BINARY_SLIM,
                    )
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
                        assembly::RUNTIME_BINARY_SLIM,
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

/// Build the package a recipe describes: resolve the recipe once into an
/// assembly request, stage the AppDir from it, then pack the result.
///
/// The request carries the resolved selection table, so the record the
/// pack lowers and the selections the assembly audited answer the same
/// resolution of the recipe's lists. The staging report prints to stderr
/// as the assembly summary: where the AppDir contents came from and what
/// the assembly did not audit.
fn run_build(
    path: &std::path::Path,
    output_override: Option<&std::path::Path>,
) -> std::io::Result<()> {
    let recipe_path = recipe::resolve(path)?;
    let recipe = recipe::load(&recipe_path)?;
    let request = assembly::AssemblyRequest::plan(&recipe_path, recipe, output_override)?;
    let report = request.stage()?;
    for line in report.summary_lines() {
        eprintln!("{line}");
    }
    let update_key = request.update.read_key()?;
    let runtime = assembly::embed_runtime(&request)?;
    pack::require_store_runtime(&runtime.note, fetches_over_https(&request))?;
    let profile = match &request.compression.profile {
        Some(p) => pack::read_profile(p)?,
        None => Vec::new(),
    };
    let mut options = request.pack_options(update_key, runtime.note, profile);
    options.metadata = entrypoint_sets_metadata(&request, &request.selections)?;
    pack::pack(&options, runtime.bytes)
}

/// Whether this build's pins are fetched over HTTPS at launch, which only
/// the update-capable runtime's client can do: the declared sets of the
/// bundle and of a named sysroot, and a GL build pinned whole. Sets and
/// pins on `file://` sources need no network stack, so they gate nothing.
fn fetches_over_https(request: &assembly::AssemblyRequest) -> bool {
    let sets = request
        .bundle
        .sets
        .iter()
        .chain(
            request
                .bundle
                .sysroot
                .as_ref()
                .map(|sysroot| sysroot.sets.iter())
                .into_iter()
                .flatten(),
        )
        .map(|set| set.url.as_str());
    let pinned = request
        .bundle
        .platform_pin
        .as_ref()
        .map(|pin| pin.url.as_str())
        .into_iter();
    sets.chain(pinned).any(|url| url.starts_with("https://"))
}

/// The metadata a build carries for its set selections: the declared
/// pin table paired with one exact selection per final manifest
/// entrypoint, in the canonical form the shared codec encodes.
///
/// A build whose every list was omitted carries none of it. The
/// runtime's documented default for absent selection metadata is
/// already every declared set, so the record would say what absence
/// already says, and the package stays byte-identical to what the same
/// recipe always packed. A build that wrote any explicit list carries
/// the complete table, because a launch reads it as the exact selection
/// and absence would read as every set.
///
/// An omitted selection expands to every declared name and a written
/// list keeps its names, so the table is complete over the final
/// entrypoint names whatever mixture of the two the recipe wrote. The
/// packer re-checks the lowered table against the final entrypoint
/// table and the recorded pins before anything is written.
fn entrypoint_sets_metadata(
    request: &assembly::AssemblyRequest,
    table: &[assembly::ResolvedSelection],
) -> std::io::Result<pack::MetadataPayload> {
    if !table.iter().any(assembly::ResolvedSelection::is_explicit) {
        return Ok(pack::MetadataPayload::default());
    }
    let declared = declared_set_pins(request)?;
    let names: std::collections::BTreeMap<&str, onelf_format::rules::RuleId> = declared
        .iter()
        .map(|pin| (pin.name.as_str(), pin.name.clone()))
        .collect();
    let mut entrypoints = Vec::new();
    for resolved in table {
        let sets: Vec<_> = match &resolved.sets {
            recipe::SetSelection::All => names.values().cloned().collect(),
            recipe::SetSelection::Names(list) => {
                let mut selected = Vec::with_capacity(list.len());
                for name in list {
                    let Some(id) = names.get(name.as_str()) else {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "entrypoint '{}': set '{name}' is not declared by this recipe",
                                resolved.name
                            ),
                        ));
                    };
                    selected.push(id.clone());
                }
                selected
            }
        };
        entrypoints.push(onelf_format::runtime_metadata::EntrypointSelection {
            name: resolved.name.clone(),
            sets,
        });
    }
    pack::lower_runtime_metadata(
        Vec::new(),
        Some(onelf_format::runtime_metadata::EntrypointSets {
            pins: declared,
            entrypoints,
        }),
    )
    .map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the entrypoint selection record cannot be written: {e}"),
        )
    })
}

/// The canonical pin table of a build's declared sets: every
/// declaration's name and full artifact hash, sorted by name. Both
/// sources come from the recipe's one `[sysroot.sets]` table, and only
/// one of them is filled, so every name is unique. A
/// declaration whose pin is not a BLAKE3 digest is refused here, so the
/// table carries only pins the published build can be checked against.
fn declared_set_pins(
    request: &assembly::AssemblyRequest,
) -> std::io::Result<Vec<onelf_format::runtime_metadata::SetPin>> {
    let own = request.bundle.sets.iter();
    let shared = request
        .bundle
        .sysroot
        .as_ref()
        .map(|sysroot| sysroot.sets.iter())
        .into_iter()
        .flatten();
    let declared: std::collections::BTreeMap<&str, &str> = own
        .chain(shared)
        .map(|set| (set.name.as_str(), set.blake3.as_str()))
        .collect();
    declared
        .into_iter()
        .map(|(name, hash)| {
            let name = onelf_format::rules::RuleId::new(name).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("set '{name}': the selection record needs an identifier: {e}"),
                )
            })?;
            let parsed = blake3::Hash::from_hex(hash).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("set '{}': pin is not a blake3 digest", name.as_str()),
                )
            })?;
            Ok(onelf_format::runtime_metadata::SetPin {
                name,
                hash: *parsed.as_bytes(),
            })
        })
        .collect()
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

#[cfg(test)]
mod selection_tests {
    use super::entrypoint_sets_metadata;
    use crate::assembly::{self, ResolvedSelection};
    use crate::metadata;
    use crate::recipe;
    use onelf_format::elf::RuntimeNote;
    use onelf_format::runtime_metadata::EntrypointSets;

    const SETS: &str = "[sysroot.sets.gtk]\npackages = [\"gtk3\"]\nurl = \"https://e/gtk.onelf\"\nblake3 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\n\n[sysroot.sets.audio]\npackages = [\"pipewire\"]\nurl = \"https://e/audio.onelf\"\nblake3 = \"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210\"\n";

    /// A recipe body: package-level keys beyond the command, then the
    /// declared sets, then any named entrypoints.
    fn recipe(package: &str, entrypoints: &str) -> String {
        format!("[package]\ncommand = \"bin/app\"\nmtime = 0\n{package}\n{SETS}\n{entrypoints}")
    }

    /// Load and plan a recipe written into a scratch directory: the
    /// exact prefix `run_build` runs, minus staging. Plan reads nothing
    /// from the tree, so the recipe file is the only file the scratch
    /// needs, and the request carries the resolved table.
    fn resolve(body: &str) -> std::io::Result<assembly::AssemblyRequest> {
        let dir = std::env::temp_dir().join(format!(
            "onelf-selection-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("onelf.toml"), body).unwrap();
        let path = recipe::resolve(&dir).unwrap();
        let recipe = recipe::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assembly::AssemblyRequest::plan(&path, recipe, None)
    }

    /// The table mirrors the manifest: the package command under its
    /// manifest name, then each named entrypoint, all selecting every
    /// declared set when no list was written.
    #[test]
    fn an_omitted_selection_resolves_over_the_final_table() {
        let request = resolve(&recipe(
            "",
            "[[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\n",
        ))
        .unwrap();
        let table = &request.selections;
        assert_eq!(table.len(), 2);
        assert_eq!(table[0].name, "app");
        assert_eq!(table[0].sets, recipe::SetSelection::All);
        assert!(!table[0].is_explicit());
        assert_eq!(table[1].name, "cli");
        assert_eq!(table[1].sets, recipe::SetSelection::All);
    }

    /// An explicit empty list keeps nothing, and a named explicit list
    /// keeps exactly its names in canonical order.
    #[test]
    fn explicit_lists_resolve_to_what_they_wrote() {
        let request = resolve(&recipe(
            "sets = []",
            "[[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\nsets = [\"gtk\"]\n",
        ))
        .unwrap();
        let table = &request.selections;
        assert_eq!(table[0].sets, recipe::SetSelection::Names(Vec::new()));
        assert!(table[0].is_explicit());
        assert_eq!(
            table[1].sets,
            recipe::SetSelection::Names(vec!["gtk".to_string()])
        );
    }

    /// The named entrypoint's omitted list selects all declared sets
    /// even though the package command wrote one: the lists are
    /// independent.
    #[test]
    fn a_named_omission_does_not_inherit_an_explicit_primary_list() {
        let request = resolve(&recipe(
            "sets = [\"gtk\"]",
            "[[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\n",
        ))
        .unwrap();
        let table = &request.selections;
        assert_eq!(
            table[0].sets,
            recipe::SetSelection::Names(vec!["gtk".to_string()])
        );
        assert_eq!(table[1].sets, recipe::SetSelection::All);
        assert!(!table[1].is_explicit());
    }

    /// A default naming a declared entrypoint folds into one record
    /// carrying that entrypoint's own selection, not a second one under
    /// the same name.
    #[test]
    fn the_default_entrypoint_carries_its_own_selection() {
        let request = resolve(&recipe(
            "",
            "[[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\ndefault = true\nsets = [\"gtk\"]\n",
        ))
        .unwrap();
        let table = &request.selections;
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].name, "cli");
        assert_eq!(
            table[0].sets,
            recipe::SetSelection::Names(vec!["gtk".to_string()])
        );
    }

    /// The command's manifest name and a named entrypoint colliding
    /// would leave which selection a launch reads up to table order, so
    /// the ambiguity is refused before the AppDir is touched.
    #[test]
    fn a_duplicate_name_in_the_final_table_is_refused() {
        let err = resolve(&recipe(
            "",
            "[[entrypoint]]\nname = \"app\"\npath = \"bin/app\"\n",
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("duplicate entrypoint name 'app'"),
            "{err}"
        );
    }

    const GTK_PIN: [u8; 32] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
        0xcd, 0xef,
    ];
    const AUDIO_PIN: [u8; 32] = [
        0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32,
        0x10, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
        0x32, 0x10,
    ];

    /// Plan a recipe written into a scratch directory, keeping the
    /// scratch: the record builder needs only the request, and the
    /// packing tests need the tree. The request carries the resolved
    /// table.
    fn planned(body: &str) -> (std::path::PathBuf, assembly::AssemblyRequest) {
        let dir = std::env::temp_dir().join(format!(
            "onelf-selection-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("onelf.toml"), body).unwrap();
        let path = recipe::resolve(&dir).unwrap();
        let recipe = recipe::load(&path).unwrap();
        let request = assembly::AssemblyRequest::plan(&path, recipe, None).unwrap();
        (dir, request)
    }

    /// The sets one entrypoint selects, by name, from a decoded record.
    fn selection<'a>(record: &'a EntrypointSets, name: &str) -> Vec<&'a str> {
        record
            .entrypoints
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.sets.iter().map(|s| s.as_str()).collect())
            .unwrap_or_else(|| panic!("no record for {name}"))
    }

    /// An explicit selection lowers the complete record: the declared
    /// pin table in canonical order, and one exact selection per final
    /// manifest entrypoint name.
    #[test]
    fn an_explicit_selection_lowers_the_complete_record() {
        let (dir, request) = planned(&recipe(
            "sets = [\"gtk\"]",
            "[[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\nsets = []\n",
        ));
        let payload = entrypoint_sets_metadata(&request, &request.selections).unwrap();
        assert!(
            payload.runtime_rules.is_none(),
            "selections carry no rule records"
        );
        let record = EntrypointSets::decode(payload.entrypoint_sets.as_deref().unwrap()).unwrap();
        assert_eq!(
            record
                .pins
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["audio", "gtk"],
            "every declared set is pinned, canonically ordered"
        );
        assert_eq!(record.pins[0].hash, AUDIO_PIN);
        assert_eq!(record.pins[1].hash, GTK_PIN);
        assert_eq!(selection(&record, "app"), ["gtk"]);
        assert_eq!(selection(&record, "cli"), [] as [&str; 0]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A legacy build, every list omitted, carries no records: absence
    /// already means every declared set, so the package stays
    /// byte-identical to what the recipe always packed.
    #[test]
    fn a_legacy_selection_lowers_no_record() {
        let (dir, request) = planned(&recipe("", ""));
        assert_eq!(
            entrypoint_sets_metadata(&request, &request.selections).unwrap(),
            crate::pack::MetadataPayload::default()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// In one complete table an omitted list expands to every declared
    /// name while a written list keeps exactly its names.
    #[test]
    fn an_omitted_list_expands_beside_a_written_one() {
        let (dir, request) = planned(&recipe(
            "sets = [\"audio\"]",
            "[[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\n",
        ));
        let payload = entrypoint_sets_metadata(&request, &request.selections).unwrap();
        let record = EntrypointSets::decode(payload.entrypoint_sets.as_deref().unwrap()).unwrap();
        assert_eq!(selection(&record, "app"), ["audio"]);
        assert_eq!(selection(&record, "cli"), ["audio", "gtk"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The entrypoint the default names keeps its recipe name in the
    /// record: renamed and default entrypoints address one record, not
    /// a second one under the command's name.
    #[test]
    fn the_default_named_entrypoint_keeps_one_record_under_its_recipe_name() {
        let (dir, request) = planned(&recipe(
            "",
            "[[entrypoint]]\nname = \"gui\"\npath = \"bin/gui\"\ndefault = true\nsets = [\"gtk\"]\n",
        ));
        let payload = entrypoint_sets_metadata(&request, &request.selections).unwrap();
        let record = EntrypointSets::decode(payload.entrypoint_sets.as_deref().unwrap()).unwrap();
        assert_eq!(
            record
                .entrypoints
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["gui"],
            "the default names the record, once"
        );
        assert_eq!(selection(&record, "gui"), ["gtk"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A selection table naming a set nothing declares is refused
    /// rather than lowered.
    #[test]
    fn a_record_names_only_declared_sets() {
        let (dir, request) = planned(&recipe("", ""));
        let hostile = vec![ResolvedSelection {
            name: "app".to_string(),
            sets: recipe::SetSelection::Names(vec!["ghost".to_string()]),
        }];
        let err = entrypoint_sets_metadata(&request, &hostile).unwrap_err();
        assert!(
            err.to_string().contains("entrypoint 'app': set 'ghost'"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A skipped tree with explicit selections packs the record through
    /// the real build onto the embedded runtime, and the packer refuses
    /// the same record on a runtime short on the selection view: the
    /// pack-level gate agrees with the selection gate.
    #[test]
    fn the_pack_gate_agrees_on_an_incapable_runtime() {
        let dir = std::env::temp_dir().join(format!(
            "onelf-selection-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/app"), "#!/bin/sh\necho hi\n").unwrap();
        let body = recipe(
            "sets = [\"gtk\"]\n\n[bundle]\nskip = true",
            "[[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\nsets = []\n",
        );
        std::fs::write(dir.join("onelf.toml"), body).unwrap();
        let path = recipe::resolve(&dir).unwrap();
        let loaded = recipe::load(&path).unwrap();
        let request = assembly::AssemblyRequest::plan(&path, loaded, None).unwrap();
        let _report = request.stage().unwrap();
        let payload = entrypoint_sets_metadata(&request, &request.selections).unwrap();

        let out = dir.join("pkg.onelf");
        let mut opts = request.pack_options(None, RuntimeNote::Absent, Vec::new());
        opts.output = out.clone();
        opts.metadata = payload;
        let err = crate::pack::pack(&opts, b"stub-runtime").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("refusing to embed a runtime"), "{text}");
        assert!(text.contains("selection-view"), "{text}");
        assert!(!out.exists(), "the refusal must come before any writing");

        let runtime = assembly::embed_runtime(&request).unwrap();
        let mut opts = request.pack_options(None, runtime.note, Vec::new());
        opts.output = out.clone();
        opts.metadata = entrypoint_sets_metadata(&request, &request.selections).unwrap();
        crate::pack::pack(&opts, runtime.bytes).unwrap();
        let carried = metadata::read_packed_file(&out, ".onelf/entrypoint-sets")
            .unwrap()
            .expect("the capable runtime carries the record");
        assert!(EntrypointSets::decode(&carried).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// End to end through the real build: a recipe with explicit
    /// selections packs a package whose selection record answers to the
    /// final entrypoint table, while the legacy recipe beside it packs
    /// no record at all.
    #[test]
    fn a_run_build_emits_the_record_its_selections_write() {
        let dir = std::env::temp_dir().join(format!(
            "onelf-selection-run-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/app"), "#!/bin/sh\necho hi\n").unwrap();
        let explicit = recipe(
            "sets = [\"gtk\"]\n\n[bundle]\nskip = true",
            "[[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\nsets = []\n",
        );
        std::fs::write(dir.join("onelf.toml"), explicit).unwrap();
        let out = dir.join("pkg.onelf");
        super::run_build(&dir.join("onelf.toml"), Some(out.as_path())).unwrap();

        let (_, manifest) = crate::info::read_footer_and_manifest(&out).unwrap();
        assert_eq!(manifest.entrypoints.len(), 2);
        let names: Vec<&str> = manifest
            .entrypoints
            .iter()
            .map(|e| manifest.get_string(e.name))
            .collect();
        assert_eq!(
            names,
            ["app", "cli"],
            "the final table the record answers to"
        );
        let carried = metadata::read_packed_file(&out, ".onelf/entrypoint-sets")
            .unwrap()
            .expect("the explicit build carries the record");
        let record = EntrypointSets::decode(&carried).unwrap();
        assert_eq!(selection(&record, "app"), ["gtk"]);
        assert_eq!(selection(&record, "cli"), [] as [&str; 0]);
        assert!(
            metadata::read_packed_file(&out, ".onelf/runtime-rules")
                .unwrap()
                .is_none(),
            "a skipped tree lowers no rule records"
        );

        let legacy = recipe(
            "\n[bundle]\nskip = true",
            "[[entrypoint]]\nname = \"cli\"\npath = \"bin/app\"\n",
        );
        std::fs::write(dir.join("onelf.toml"), legacy).unwrap();
        let plain = dir.join("plain.onelf");
        super::run_build(&dir.join("onelf.toml"), Some(plain.as_path())).unwrap();
        assert!(
            metadata::read_packed_file(&plain, ".onelf/entrypoint-sets")
                .unwrap()
                .is_none(),
            "the legacy build emits nothing"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
