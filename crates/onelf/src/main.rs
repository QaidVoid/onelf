mod bundle;
mod cache;
mod compress;
mod extract;
mod info;
mod list;
mod metadata;
mod pack;
mod recipe;

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

        /// Exclude files matching glob patterns (repeatable, e.g. "*.a", "__pycache__")
        #[arg(long)]
        exclude: Vec<String>,
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

    /// Manage the onelf cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

    /// Bundle shared library dependencies into a directory
    BundleLibs {
        /// Directory containing the application to bundle
        directory: PathBuf,

        /// Specific binary to analyze (default: scan all ELF files)
        #[arg(long)]
        target: Option<PathBuf>,

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

        /// Strip debug symbols from copied libraries
        #[arg(long)]
        strip: bool,

        /// Skip libraries whose libc family (musl vs glibc) doesn't match
        /// the target binary. Without this flag, mismatched libs are copied
        /// with a warning.
        #[arg(long)]
        strict_libc: bool,
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
            memfd,
            no_memfd,
            working_dir,
            update_url,
            exclude,
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
                    use_dict: dict,
                    memfd: memfd_opt,
                    working_dir: wd,
                    update_url: update_url.clone(),
                    exclude,
                },
                // Pick the runtime: slim (~700KB) by default; the
                // update-capable runtime (~2MB) only when the user actually
                // configures self-updates.
                if update_url.is_some() {
                    RUNTIME_BINARY_UPDATE
                } else {
                    RUNTIME_BINARY_SLIM
                },
            )
        }
        Commands::Build { path, output } => run_build(&path, output.as_deref()),
        Commands::Info { binary } => info::info(&binary),
        Commands::List { binary } => list::list(&binary),
        Commands::Extract {
            binary,
            output,
            file,
        } => extract::extract(&binary, output.as_deref(), &file),
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
        Commands::Cache { action } => match action {
            CacheAction::List => cache::cache_list(),
            CacheAction::Clear => cache::cache_clear(),
            CacheAction::Gc { max_age } => cache::cache_gc(max_age),
        },
        Commands::BundleLibs {
            directory,
            target,
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
            strip,
            strict_libc,
        } => bundle::bundle_libs(&bundle::BundleOptions {
            directory,
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
            strip,
            strict_libc,
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
    let recipe_path = recipe::resolve(path);
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
            .map(|s| PathBuf::from(recipe::expand_env(s)))
            .collect();

        bundle::bundle_libs(&bundle::BundleOptions {
            directory: dir.clone(),
            target: None,
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
            strip: recipe.bundle.strip,
            strict_libc: recipe.bundle.strict_libc,
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
    let runtime: &[u8] = if update_url.is_some() {
        RUNTIME_BINARY_UPDATE
    } else {
        RUNTIME_BINARY_SLIM
    };

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
            memfd: recipe.package.memfd,
            working_dir: recipe.package.working_dir.into(),
            update_url,
            exclude: recipe.package.exclude,
        },
        runtime,
    )
}
