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

        /// Library directories to add to LD_LIBRARY_PATH (repeatable).
        /// Pass "auto" (default) to detect directories containing .so files.
        #[arg(long, default_values_t = [String::from("auto")])]
        lib_dir: Vec<String>,

        /// Zstd compression level (0-22)
        #[arg(long, default_value = "12")]
        level: i32,

        /// Build a shared zstd dictionary
        #[arg(long)]
        dict: bool,

        /// Store the payload uncompressed (no zstd). Larger file, but
        /// zero decompression at runtime. Overrides --dict.
        #[arg(long)]
        no_compress: bool,

        /// Mark default entrypoint as memfd-eligible
        #[arg(long)]
        memfd: bool,

        /// Force cache mode (disable memfd)
        #[arg(long)]
        no_memfd: bool,

        /// Working directory strategy
        #[arg(long, default_value = "inherit")]
        working_dir: WorkingDirArg,

        /// Base URL for delta updates (stored in .onelf/update-url)
        #[arg(long)]
        update_url: Option<String>,

        /// Path to a file holding the raw 32-byte Ed25519 public key used
        /// to verify signed self-updates (stored in .onelf/update-key)
        #[arg(long)]
        update_key: Option<PathBuf>,

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

        /// Entrypoint name (default: default entrypoint)
        #[arg(long)]
        entrypoint: Option<String>,
    },

    /// Remove desktop shortcut and icon installed by integrate
    Unintegrate {
        /// Path to the onelf binary
        binary: PathBuf,

        /// Entrypoint name (default: default entrypoint)
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

        /// Where to write the signature (default: <BINARY>.sig)
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
        #[arg(long)]
        gl: bool,

        /// Bundle DRI drivers (architecture-filtered)
        #[arg(long)]
        dri: bool,

        /// Bundle Vulkan ICD drivers (architecture-filtered)
        #[arg(long)]
        vulkan: bool,

        /// Bundle Wayland client libraries (libwayland, libdecor, libxkbcommon)
        #[arg(long)]
        wayland: bool,

        /// Bundle GSettings schemas for GTK apps
        #[arg(long)]
        gtk: bool,

        /// Do not bundle Mesa GL/EGL/GBM libraries even if auto-detected
        #[arg(long)]
        no_gl: bool,

        /// Do not bundle DRI drivers even if auto-detected
        #[arg(long)]
        no_dri: bool,

        /// Do not bundle Vulkan ICD drivers even if auto-detected
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
            dict,
            no_compress,
            memfd,
            no_memfd,
            working_dir,
            update_url,
            update_key,
            no_embed_updater,
            exclude,
            preload,
            needs_setuid,
            mtime,
        } => {
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
            match update_key {
                Err(e) => Err(e),
                Ok(update_key) => pack::pack(
                    &pack::PackOptions {
                        directory,
                        output,
                        command,
                        name,
                        entrypoints,
                        default_entrypoint,
                        lib_dirs: lib_dir,
                        level,
                        use_dict: dict,
                        no_compress,
                        memfd: memfd_opt,
                        working_dir: wd,
                        update_url: update_url.clone(),
                        embed_updater: !no_embed_updater,
                        update_key,
                        exclude,
                        package_info: None,
                        mtime,
                        env: Vec::new(),
                        preload,
                        needs_setuid,
                    },
                    // Pick the runtime: slim (~700KB) by default; the
                    // update-capable runtime (~2MB) only when the user actually
                    // configures self-updates.
                    if update_url.is_some() && !no_embed_updater {
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
        Commands::Verify { binary } => verify::verify(&binary),
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
        } => scaffold_from_binary(&directory, from_binary.as_deref()).and_then(|_| {
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
            })
        }),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run_build(
    path: &std::path::Path,
    output_override: Option<&std::path::Path>,
) -> std::io::Result<()> {
    let recipe_path = recipe::resolve(path)?;
    let recipe = recipe::load(&recipe_path)?;
    let dir = recipe_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // Stage 1: bundle-libs
    if !recipe.bundle.skip {
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
    let embed_updater = recipe.update.as_ref().and_then(|u| u.embed).unwrap_or(true);
    let runtime: &[u8] = if update_url.is_some() && embed_updater {
        RUNTIME_BINARY_UPDATE
    } else {
        RUNTIME_BINARY_SLIM
    };

    let package_info = build_package_info(&recipe.package);

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
            use_dict: recipe.compression.dict,
            no_compress: recipe.compression.store,
            memfd: recipe.package.memfd,
            working_dir: recipe.package.working_dir.into(),
            update_url,
            embed_updater,
            update_key,
            exclude: recipe.package.exclude,
            package_info,
            mtime: recipe.package.mtime,
            env: recipe.env.into_iter().collect(),
            preload: recipe.preload,
            needs_setuid: recipe.package.needs_setuid,
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
