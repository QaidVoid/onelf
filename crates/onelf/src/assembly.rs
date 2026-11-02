//! Normalized assembly request and report for recipe-driven builds.
//!
//! The build path resolves a loaded recipe once into an
//! [`AssemblyRequest`]: every recipe-relative path is resolved against
//! the recipe's directory, defaults are applied, and declarations that
//! cannot apply are refused before the AppDir is touched. Staging the
//! AppDir from a request produces an [`AssemblyReport`], and the same
//! request yields the low-level pack invocation.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use onelf_format::WorkingDir;
use onelf_format::elf;

use crate::bundle;
use crate::pack;
use crate::recipe;
use crate::sysroot_cmd;

/// The slim runtime: runs the packaged application with no updater.
pub(crate) const RUNTIME_BINARY_SLIM: &[u8] = include_bytes!(env!("ONELF_RT_PATH"));

/// The update-capable runtime: the slim runtime plus self-update
/// fetching and verification.
pub(crate) const RUNTIME_BINARY_UPDATE: &[u8] = include_bytes!(env!("ONELF_RT_UPDATE_PATH"));

/// A named entrypoint from the recipe: the name the runtime addresses
/// it by, its path in the AppDir, and the arguments it starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entrypoint {
    /// The name the runtime and the CLI address the entrypoint by.
    pub name: String,
    /// The entrypoint path relative to the AppDir.
    pub path: String,
    /// Arguments prepended to the entrypoint's own.
    pub args: Vec<String>,
}

/// Package-level fields the packer consumes, normalized from the
/// recipe's `[package]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRequest {
    /// The entrypoint path relative to the AppDir, as in the recipe.
    pub command: String,
    /// The package name, when the recipe names one.
    pub name: Option<String>,
    /// TOML metadata rendered into `.onelf/package-info.toml`.
    pub package_info: Option<String>,
    /// Mark the default entrypoint memfd-eligible, overriding
    /// auto-detection.
    pub memfd: Option<bool>,
    /// What working directory the entrypoint starts in.
    pub working_dir: WorkingDir,
    /// What the launch resolver may take from the host. `auto` decides
    /// from the bundle's contents.
    pub host_libs: pack::HostLibs,
    /// Let the runtime fall back to the persistent cache when no other
    /// execution mode works.
    pub cache: bool,
    /// Run from the persistent cache before any other execution mode.
    /// Implies `cache`.
    pub prefer_cache: bool,
    /// Keep the app out of user namespaces so setuid binaries work.
    pub needs_setuid: bool,
    /// Paths excluded from the payload, as written in the recipe.
    pub exclude: Vec<String>,
    /// Pin every entry's mtime for reproducible output. A sysroot build
    /// pins zero unless the recipe or `SOURCE_DATE_EPOCH` asks for a
    /// time.
    pub mtime: Option<u64>,
}

/// Compression and payload settings, normalized from the recipe's
/// `[compression]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionRequest {
    /// Zstd compression level.
    pub level: i32,
    /// Bytes per payload block, defaulted per payload kind.
    pub block_size: u64,
    /// What the payload is made of.
    pub payload: pack::Payload,
    /// Build a shared zstd dictionary.
    pub use_dict: bool,
    /// Store the payload uncompressed. Larger file, zero decompression
    /// at runtime.
    pub no_compress: bool,
    /// The startup profile file, resolved against the recipe directory.
    pub profile: Option<PathBuf>,
}

/// Self-update settings, normalized from the recipe's `[update]`
/// section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRequest {
    /// The URL the runtime fetches updates from.
    pub url: Option<String>,
    /// The signing key file, resolved against the recipe directory.
    pub key_path: Option<PathBuf>,
    /// Release number stamped into the package for rollback protection.
    pub revision: Option<u64>,
    /// Accept unsigned self-updates: no signing key is stored.
    pub unsigned: bool,
    /// Embed the update-capable runtime when a URL is set. Defaults to
    /// true.
    pub embed: bool,
}

impl UpdateRequest {
    /// Read the signing key bytes from the resolved path, for the packer
    /// to embed.
    pub fn read_key(&self) -> io::Result<Option<Vec<u8>>> {
        self.key_path
            .as_ref()
            .map(|path| {
                std::fs::read(path).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("recipe [update] key {}: {e}", path.display()),
                    )
                })
            })
            .transpose()
    }
}

/// The bare binary a recipe imports before assembly, resolved against
/// the recipe's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryImport {
    /// The file to import, as the build reads it.
    pub source: PathBuf,
}

/// How the AppDir contents are produced, normalized from the recipe's
/// `[bundle]` and `[sysroot]` sections.
#[derive(Debug, Clone)]
pub struct BundleRequest {
    /// Run no bundler: the AppDir stands as authored and is reported as
    /// unchecked. Never set when a sysroot database applies, which
    /// populates the AppDir in its own step.
    pub skip: bool,
    pub gl: bool,
    pub dri: bool,
    pub vulkan: bool,
    pub wayland: bool,
    pub gtk: bool,
    /// Opt out of a framework even when auto-detection or an explicit
    /// flag would enable it. Opt-out wins over both.
    pub no_gl: bool,
    pub no_dri: bool,
    pub no_vulkan: bool,
    pub no_wayland: bool,
    pub no_gtk: bool,
    /// Strip debug symbols from bundled libraries.
    pub strip: bool,
    /// Refuse libraries that do not match the entrypoint's libc family.
    pub strict_libc: bool,
    /// Scan the entrypoint for dlopen'd libraries.
    pub scan_dlopen: bool,
    /// Extra sonames added to the dlopen scan allow-list.
    pub dlopen_extra: Vec<String>,
    /// Run the entrypoint and bundle every library it loaded at runtime.
    pub trace_run: bool,
    /// How long the traced run may go before it is stopped; 5 by
    /// default.
    pub trace_seconds: u64,
    /// Arguments for the traced run.
    pub trace_args: Vec<String>,
    /// Extra host directories libraries are searched in, resolved
    /// against the recipe directory.
    pub search_paths: Vec<PathBuf>,
    /// Paths excluded from bundling, as written in the recipe.
    pub exclude: Vec<String>,
    /// Paths force-included into the bundle, as written in the recipe.
    pub include: Vec<String>,
    /// The sysroot package database the bundle contents are taken from.
    /// `None` leaves the AppDir to the host scan or to the recipe's skip
    /// flag.
    pub sysroot: Option<bundle::SysrootOptions>,
    /// Shared sets recorded for the runtime to fetch at launch.
    pub sets: Vec<bundle::sysroot::SharedSet>,
    /// A GL build pinned whole by the recipe.
    pub platform_pin: Option<onelf_sysroot::platform::Pin>,
}

/// Where an assembled AppDir's contents came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The bundler scanned the entrypoint and this machine.
    HostScan,
    /// The bundler took the contents from a sysroot package database.
    SysrootDatabase,
    /// The recipe skipped bundling: the tree stands as authored, with no
    /// scan and no verification.
    Unchecked,
}

/// What assembly did, normalized so reports can present outcomes
/// deterministically instead of re-deriving them from prose output.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssemblyReport {
    /// Where the AppDir contents came from.
    pub source: Source,
    /// Frameworks the recipe requested when they reached the host scan,
    /// in the fixed order the build names them.
    pub frameworks: Vec<&'static str>,
    /// Names of the shared sets recorded for the runtime to fetch, in
    /// recipe order.
    pub sets: Vec<String>,
    /// Framework descriptor rules whose soname triggers matched during
    /// the walk, in catalog order, each naming the soname that activated
    /// it and the resource groups queued for it.
    pub active_rules: Vec<bundle::RuleActivation>,
    /// Whether a whole GL build was pinned.
    pub pinned_gl: bool,
}

/// The normalized inputs for one recipe-driven assembly: everything the
/// build path derives from a loaded recipe and its output flag, resolved
/// against the recipe's directory, before the AppDir is touched.
#[derive(Debug)]
pub struct AssemblyRequest {
    /// The AppDir the package is assembled from.
    pub directory: PathBuf,
    /// Where the finished executable is written, resolved against the
    /// recipe directory unless given outright.
    pub output: PathBuf,
    /// Package-level fields the packer consumes.
    pub package: PackageRequest,
    /// Named entrypoints beyond the package command.
    pub entrypoints: Vec<Entrypoint>,
    /// The entrypoint the recipe marked default, if any.
    pub default_entrypoint: Option<String>,
    /// Library directories reported to the packer, `auto` by default.
    pub lib_dirs: Vec<String>,
    /// Compression and payload settings.
    pub compression: CompressionRequest,
    /// Self-update settings.
    pub update: UpdateRequest,
    /// Environment variables set before exec, sorted by key.
    pub env: Vec<(String, String)>,
    /// Host path to bundle-directory overlays, sorted by host path.
    pub paths: Vec<(String, String)>,
    /// Libraries dlopen'd on every exec by the bundled environment
    /// constructor.
    pub preload: Vec<String>,
    /// The bare binary imported to the package command's path before
    /// the AppDir is assembled, when the recipe asks for one.
    pub input: Option<BinaryImport>,
    /// How the AppDir contents are produced.
    pub bundle: BundleRequest,
}

impl AssemblyRequest {
    /// Normalize a loaded recipe and the build flags into an assembly
    /// request. Recipe-relative paths resolve against the recipe's
    /// directory, and flags that cannot apply to the chosen inputs are
    /// refused before anything is resolved or mutated.
    pub fn plan(
        recipe_path: &Path,
        recipe: recipe::Recipe,
        output_override: Option<&Path>,
    ) -> io::Result<Self> {
        let dir = recipe_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let recipe::Recipe {
            package,
            entrypoint,
            compression,
            update,
            bundle: rb,
            sysroot,
            env,
            paths,
            preload,
            input,
        } = recipe;

        // An imported binary is a second authoritative source of the
        // AppDir's contents, so it is judged before anything is
        // materialized or mutated: a conflict must fail naming both
        // fields while neither source has been touched.
        let import = match input
            .as_ref()
            .and_then(|i| i.binary.as_ref().map(|b| (i, b)))
        {
            None => None,
            Some((declared, binary)) => {
                declared.validate()?;
                if sysroot.as_ref().is_some_and(|sr| sr.path.is_some()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "{}: [input] binary and [sysroot] path conflict: a build \
                             takes its contents from one authoritative source, so \
                             name one of them",
                            recipe_path.display()
                        ),
                    ));
                }
                if rb.skip {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "{}: [input] binary conflicts with [bundle] skip: skip \
                             expects an already prepared tree, so there is nothing \
                             to import into",
                            recipe_path.display()
                        ),
                    ));
                }
                import_destination(&package.command)?;
                let source = dir.join(binary);
                let why = "[input] binary names a bare executable file to import; \
                           archives, package-manager identifiers and source trees \
                           are not supported inputs";
                let meta = std::fs::metadata(&source).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "{}: [input] binary {}: {e}. {why}",
                            recipe_path.display(),
                            source.display()
                        ),
                    )
                })?;
                if !meta.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "{}: [input] binary {}: not a file. {why}",
                            recipe_path.display(),
                            source.display()
                        ),
                    ));
                }
                Some(BinaryImport { source })
            }
        };

        // A build from a sysroot's package database does not use the
        // framework bundlers, so their flags would do nothing. A recipe
        // refuses them the same way `--sysroot` does. A `[sysroot]`
        // without `path` leaves the AppDir to the host scan, where the
        // flags keep working.
        let frameworks = framework_names(rb.gl, rb.dri, rb.vulkan, rb.wayland, rb.gtk);
        let sysroot_db = sysroot.as_ref().is_some_and(|sr| sr.path.is_some());
        if sysroot_db && !frameworks.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{}: [bundle] {} cannot be used with [sysroot]. A sysroot \
                     build takes the bundle's contents from the sysroot and does \
                     not use the framework bundlers",
                    recipe_path.display(),
                    frameworks.join(", ")
                ),
            ));
        }

        // A sysroot build is reproducible from its archive and recipe
        // alone, so the time the AppDir was written does not reach the
        // package unless the recipe or SOURCE_DATE_EPOCH asks for a time.
        let mtime = package
            .mtime
            .or((sysroot.is_some() && pack::source_date_epoch().is_none()).then_some(0));

        let (sysroot, sets, platform_pin) = match sysroot {
            Some(sr) => {
                let resolved = sysroot_from_recipe(&dir, &package.command, &sr)?;
                (resolved.root, resolved.sets, resolved.pin)
            }
            None => (None, Vec::new(), None),
        };

        // Relative output paths in the recipe are resolved against the
        // recipe's directory.
        let output = match output_override {
            Some(o) => o.to_path_buf(),
            None => match package.output.clone() {
                Some(o) if o.is_absolute() => o,
                Some(o) => dir.join(o),
                None => {
                    let name = package.name.clone().unwrap_or_else(|| {
                        package
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

        let default_entrypoint = entrypoint
            .iter()
            .find(|e| e.default)
            .map(|e| e.name.clone());
        let entrypoints: Vec<Entrypoint> = entrypoint
            .into_iter()
            .map(|e| Entrypoint {
                name: e.name,
                path: e.path,
                args: e.args,
            })
            .collect();
        let lib_dirs = rb
            .lib_dirs
            .clone()
            .unwrap_or_else(|| vec!["auto".to_string()]);

        let update_url = update.as_ref().map(|u| u.url.clone());
        let update_key_path = update
            .as_ref()
            .and_then(|u| u.key.clone())
            .map(|k| if k.is_absolute() { k } else { dir.join(k) });
        let update_revision = update.as_ref().and_then(|u| u.revision);
        let update_unsigned = update.as_ref().map(|u| u.unsigned).unwrap_or(false);
        let embed_updater = update.as_ref().and_then(|u| u.embed).unwrap_or(true);

        let package_info = build_package_info(&package);
        let profile = compression.prefetch.map(|p| dir.join(p));
        let payload: pack::Payload = compression.payload.into();

        Ok(AssemblyRequest {
            directory: dir,
            output,
            package: PackageRequest {
                command: package.command,
                name: package.name,
                package_info,
                memfd: package.memfd,
                working_dir: package.working_dir.into(),
                host_libs: package
                    .host_libs
                    .map(Into::into)
                    .unwrap_or(pack::HostLibs::Auto),
                cache: package.cache,
                prefer_cache: package.prefer_cache,
                needs_setuid: package.needs_setuid,
                exclude: package.exclude,
                mtime,
            },
            entrypoints,
            default_entrypoint,
            lib_dirs,
            compression: CompressionRequest {
                level: compression.level,
                block_size: compression
                    .block_size
                    .unwrap_or(payload.default_block_size()),
                payload,
                use_dict: compression.dict,
                no_compress: compression.store,
                profile,
            },
            update: UpdateRequest {
                url: update_url,
                key_path: update_key_path,
                revision: update_revision,
                unsigned: update_unsigned,
                embed: embed_updater,
            },
            env: env.into_iter().collect(),
            paths: paths.into_iter().collect(),
            preload,
            input: import,
            bundle: BundleRequest {
                skip: rb.skip && sysroot.is_none(),
                gl: rb.gl,
                dri: rb.dri,
                vulkan: rb.vulkan,
                wayland: rb.wayland,
                gtk: rb.gtk,
                no_gl: rb.no_gl,
                no_dri: rb.no_dri,
                no_vulkan: rb.no_vulkan,
                no_wayland: rb.no_wayland,
                no_gtk: rb.no_gtk,
                strip: rb.strip,
                strict_libc: rb.strict_libc,
                scan_dlopen: rb.scan_dlopen,
                dlopen_extra: rb.dlopen,
                trace_run: rb.trace_run,
                trace_seconds: rb.trace_seconds.unwrap_or(5),
                trace_args: rb.trace_args,
                search_paths: rb.search_paths.into_iter().map(PathBuf::from).collect(),
                exclude: rb.exclude,
                include: rb.include,
                sysroot,
                sets,
                platform_pin,
            },
        })
    }

    /// Populate the AppDir this request names and report what the
    /// assembly did.
    ///
    /// A recipe that imports a bare binary has it copied to the
    /// package command's path first, so a build can start from
    /// nothing but the binary. A sysroot is applied by bundle-libs, so
    /// it runs even when the recipe would otherwise skip bundling. A
    /// recipe that skips bundling and pins sets without a sysroot
    /// keeps its AppDir exactly as it stands: only the records the
    /// runtime fetches by are written, with no scan and no verification.
    pub fn stage(&self) -> io::Result<AssemblyReport> {
        if let Some(import) = &self.input {
            import_binary(&self.directory, import, &self.package.command)?;
        }
        let sets = set_names(&self.bundle.sets);
        if self.bundle.skip {
            if !self.bundle.sets.is_empty() || self.bundle.platform_pin.is_some() {
                bundle::sysroot::record_sets(
                    &self.directory,
                    &self.bundle.sets,
                    self.bundle.platform_pin.as_ref(),
                )?;
            }
            return Ok(AssemblyReport {
                source: Source::Unchecked,
                frameworks: Vec::new(),
                sets,
                pinned_gl: self.bundle.platform_pin.is_some(),
                active_rules: Vec::new(),
            });
        }
        bundle::bundle_libs(&self.bundle_options())?;
        if self.input.is_some() {
            // The bundler may rewrite the imported command (its ELF
            // constructor injection among other things), so the import's
            // recorded hash is attested only after that rewriting: what
            // the record then hashes are the build's final bytes.
            bundle::sysroot::attest_generated(&self.directory)?;
        }
        Ok(AssemblyReport {
            source: if self.bundle.sysroot.is_some() {
                Source::SysrootDatabase
            } else {
                Source::HostScan
            },
            frameworks: if self.bundle.sysroot.is_none() {
                framework_names(
                    self.bundle.gl,
                    self.bundle.dri,
                    self.bundle.vulkan,
                    self.bundle.wayland,
                    self.bundle.gtk,
                )
            } else {
                Vec::new()
            },
            sets,
            pinned_gl: self.bundle.platform_pin.is_some(),
            active_rules: bundle::take_activations(),
        })
    }

    /// The bundle-libs invocation this request normalizes to.
    fn bundle_options(&self) -> bundle::BundleOptions {
        bundle::BundleOptions {
            directory: self.directory.clone(),
            target: None,
            primary: Some(PathBuf::from(&self.package.command)),
            lib_dir: PathBuf::from("lib"),
            exclude: self.bundle.exclude.clone(),
            include: self.bundle.include.clone(),
            search_path: self.bundle.search_paths.clone(),
            dry_run: false,
            recursive: true,
            gl: self.bundle.gl,
            dri: self.bundle.dri,
            vulkan: self.bundle.vulkan,
            wayland: self.bundle.wayland,
            gtk: self.bundle.gtk,
            no_gl: self.bundle.no_gl,
            no_dri: self.bundle.no_dri,
            no_vulkan: self.bundle.no_vulkan,
            no_wayland: self.bundle.no_wayland,
            no_gtk: self.bundle.no_gtk,
            strip: self.bundle.strip,
            strict_libc: self.bundle.strict_libc,
            scan_dlopen: self.bundle.scan_dlopen,
            dlopen_extra: self.bundle.dlopen_extra.clone(),
            trace_run: self.bundle.trace_run,
            trace_seconds: self.bundle.trace_seconds,
            trace_args: self.bundle.trace_args.clone(),
            sysroot: self.bundle.sysroot.clone(),
            sets: self.bundle.sets.clone(),
            platform_pin: self.bundle.platform_pin.clone(),
        }
    }

    /// The low-level pack invocation this request resolves to.
    pub fn pack_options(
        &self,
        update_key: Option<Vec<u8>>,
        runtime_note: elf::RuntimeNote,
        profile: Vec<(String, u64)>,
    ) -> pack::PackOptions {
        pack::PackOptions {
            directory: self.directory.clone(),
            output: self.output.clone(),
            command: self.package.command.clone(),
            name: self.package.name.clone(),
            entrypoints: self
                .entrypoints
                .iter()
                .map(|e| (e.name.clone(), e.path.clone(), e.args.clone()))
                .collect(),
            default_entrypoint: self.default_entrypoint.clone(),
            lib_dirs: self.lib_dirs.clone(),
            level: self.compression.level,
            block_size: self.compression.block_size,
            payload: self.compression.payload,
            use_dict: self.compression.use_dict,
            no_compress: self.compression.no_compress,
            memfd: self.package.memfd,
            working_dir: self.package.working_dir,
            host_libs: self.package.host_libs,
            cache: self.package.cache,
            prefer_cache: self.package.prefer_cache,
            update_url: self.update.url.clone(),
            embed_updater: self.update.embed,
            update_key,
            update_revision: self.update.revision,
            update_unsigned: self.update.unsigned,
            exclude: self.package.exclude.clone(),
            package_info: self.package.package_info.clone(),
            mtime: self.package.mtime,
            env: self.env.clone(),
            paths: self.paths.clone(),
            preload: self.preload.clone(),
            needs_setuid: self.package.needs_setuid,
            profile,
            provider_index: None,
            // Production emission stays unwired: no build lowers the
            // report's active rules into the metadata records yet, so
            // every package carries neither record.
            metadata: pack::MetadataPayload::default(),
            runtime_note,
        }
    }
}

/// The AppDir path an import writes to: the package command as a plain
/// relative path. Traversal, an absolute prefix or anything else that
/// could reach outside the AppDir is refused before anything is staged.
fn import_destination(command: &str) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(command).components() {
        match component {
            Component::Normal(part) => out.push(part),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "command path '{command}' must be a plain relative path \
                         inside the AppDir"
                    ),
                ));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("command path '{command}' names nothing inside the AppDir"),
        ));
    }
    Ok(out)
}

/// Refuse to write `rel` under `appdir` when any existing component of
/// it, including the last, is a symbolic link: a link could carry the
/// write outside the AppDir or over a file the import does not own. A
/// component that does not exist yet ends the walk; the import creates
/// the missing directories itself.
fn refuse_destination_links(appdir: &Path, rel: &Path) -> io::Result<()> {
    let mut prefix = PathBuf::from(appdir);
    for component in rel.components() {
        prefix.push(component);
        match std::fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{}: a symbolic link is in the way of the import; remove \
                         it or point [package] command elsewhere",
                        prefix.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("{}: {e}", prefix.display()),
                ));
            }
        }
    }
    Ok(())
}

/// The BLAKE3 of a regular file's bytes, tagged the way
/// [`bundle::sysroot::hash_entry`] tags a file, so a staged copy, its
/// recorded hash and its source all compare under one scheme.
fn hash_file(path: &Path) -> io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"file ");
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(hasher.finalize())
}

/// Whether an existing destination may be overwritten by this import.
/// Bytes that already match the source are an idempotent re-import.
/// Bytes the record still attests as the previous build's own output
/// are a refresh, and so is a record whose hash predates the build's
/// own ELF rewriting, since the rewriting may have changed the bytes
/// through no publisher's hand. Anything else is publisher content,
/// which stays and stops the build instead of being replaced or packed.
fn judge_existing_import(
    appdir: &Path,
    dest_rel: &Path,
    actual: blake3::Hash,
    snapshot: blake3::Hash,
) -> io::Result<()> {
    if actual == snapshot {
        return Ok(());
    }
    let dest_rel = dest_rel.to_string_lossy();
    let recorded = bundle::sysroot::read_generated(appdir)?
        .into_iter()
        .find(|e| e.path == dest_rel);
    match recorded {
        Some(entry) if entry.pending => Ok(()),
        Some(entry) if entry.hash == Some(actual) => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{dest_rel}: holds bytes this recipe did not import: neither the \
                 recorded import nor the source matches. Remove the file or move \
                 it to a name [package] command does not use"
            ),
        )),
    }
}

/// Copy `source` beside `dest` under an exclusively created unique name,
/// for the caller to hash, make executable, and rename into place.
fn staged_copy(source: &Path, dest: &Path) -> io::Result<PathBuf> {
    let dir = dest.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: no parent directory to stage the import in",
                dest.display()
            ),
        )
    })?;
    let name = dest.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: no file name to stage the import under", dest.display()),
        )
    })?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staged = dir.join(format!(
        ".{}.import-{}-{nanos}",
        name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::copy(source, &staged).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("[input] binary {}: {e}", source.display()),
        )
    })?;
    Ok(staged)
}

/// Import a recipe's bare binary into the AppDir: copy the source to
/// the package command's path through a uniquely named staged file
/// renamed into place. The source is only ever read, so an alias or a
/// hardlink to it never changes, and no destination a symlink names is
/// ever written through.
///
/// The copy is recorded in the generated ledger with its content hash.
/// The record starts pending: the build may still rewrite the command
/// after the import, and only the bytes the build finishes with are
/// attested. A staged copy the record still attests is refreshed from
/// the source, including a source that changed upstream since the last
/// build; a record whose hash predates the build's own rewriting is
/// also refreshed; staged bytes that attest to nothing are publisher
/// content and stop the build with a conflict before anything is packed.
fn import_binary(appdir: &Path, import: &BinaryImport, command: &str) -> io::Result<()> {
    let dest_rel = import_destination(command)?;
    let dest = appdir.join(&dest_rel);
    refuse_destination_links(appdir, &dest_rel)?;
    let snapshot = hash_file(&import.source)?;
    match std::fs::symlink_metadata(&dest) {
        Ok(meta) if meta.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{}: is a directory; [package] command must name a file the \
                     import can write",
                    dest.display()
                ),
            ));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: not a regular file; refusing to import over it",
                    dest.display()
                ),
            ));
        }
        Ok(_) => judge_existing_import(appdir, &dest_rel, hash_file(&dest)?, snapshot)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", dest.display()))),
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged = staged_copy(&import.source, &dest)?;
    if hash_file(&staged)? != snapshot {
        let _ = std::fs::remove_file(&staged);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: changed while the import copied it",
                import.source.display()
            ),
        ));
    }
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staged, &dest)?;
    bundle::sysroot::record_generated(appdir, vec![dest_rel.to_string_lossy().into_owned()])?;
    Ok(())
}

/// The runtime selected for a build, with the capability note probed
/// from its actual bytes.
pub struct Runtime {
    /// The runtime's bytes, ready to embed.
    pub bytes: &'static [u8],
    /// What those bytes advertise. A note that exists but cannot be read
    /// fails the build.
    pub note: elf::RuntimeNote,
}

/// Select the runtime a build embeds and probe its capability note from
/// the actual bytes: the update-capable one when the package self-updates
/// over HTTPS or the AppDir pins an HTTPS build only that runtime can
/// fetch, the slim one otherwise.
pub fn embed_runtime(request: &AssemblyRequest) -> io::Result<Runtime> {
    let bytes = if (request.update.url.is_some() && request.update.embed)
        || pins_https_build(&request.directory)
    {
        RUNTIME_BINARY_UPDATE
    } else {
        RUNTIME_BINARY_SLIM
    };
    Ok(Runtime {
        bytes,
        note: elf::probe_capability_note(bytes)?,
    })
}

/// Whether the AppDir pins a GL build over HTTPS, which only the
/// update-capable runtime can fetch.
pub(crate) fn pins_https_build(dir: &Path) -> bool {
    [bundle::sysroot::PLATFORM_FILE, bundle::sysroot::SETS_FILE]
        .iter()
        .any(|f| {
            std::fs::read_to_string(dir.join(f))
                .map(|t| t.lines().any(|l| l.starts_with("url = \"https://")))
                .unwrap_or(false)
        })
}

/// What a recipe's `[sysroot]` section resolves to for a build.
struct ResolvedSysroot {
    /// The package database the bundle's contents are taken from. `None`
    /// when the recipe names no `path`: the AppDir then stands as the
    /// author assembled it.
    root: Option<bundle::SysrootOptions>,
    /// The sets recorded for the runtime to fetch at launch.
    sets: Vec<bundle::sysroot::SharedSet>,
    /// A GL build pin, when the recipe names one whole.
    pin: Option<onelf_sysroot::platform::Pin>,
}

/// The sysroot a recipe names, materialized from its archive when the
/// directory does not exist yet. Paths resolve against the recipe's
/// directory. Without a `path` there is no database to consult: only the
/// sets and the pin are carried, and `validate` has refused everything
/// else.
fn sysroot_from_recipe(
    dir: &Path,
    command: &str,
    sr: &recipe::Sysroot,
) -> io::Result<ResolvedSysroot> {
    let sets = sr
        .sets
        .iter()
        .map(|(name, set)| bundle::sysroot::SharedSet {
            name: name.clone(),
            packages: set.packages.clone(),
            url: set.url.clone(),
            blake3: set.blake3.clone(),
        })
        .collect();
    let Some(path) = &sr.path else {
        return Ok(ResolvedSysroot {
            root: None,
            sets,
            pin: bundle::sysroot::pin_without_root(
                sr.platform_url.as_ref(),
                sr.platform_hash.as_ref(),
            )?,
        });
    };
    let resolve = |p: &Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            dir.join(p)
        }
    };
    let root = resolve(path);
    bundle::sysroot::refuse_inside(&root, dir)?;
    if !root.is_dir() {
        let Some(archive) = &sr.archive else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
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
    Ok(ResolvedSysroot {
        root: Some(bundle::SysrootOptions {
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
            sets,
        }),
        sets: Vec::new(),
        pin: None,
    })
}

/// Framework names for the flags a recipe sets, in the fixed order the
/// build names them.
fn framework_names(
    gl: bool,
    dri: bool,
    vulkan: bool,
    wayland: bool,
    gtk: bool,
) -> Vec<&'static str> {
    [
        ("gl", gl),
        ("dri", dri),
        ("vulkan", vulkan),
        ("wayland", wayland),
        ("gtk", gtk),
    ]
    .into_iter()
    .filter_map(|(name, on)| on.then_some(name))
    .collect()
}

/// Names of the shared sets a build records, in recipe order.
fn set_names(sets: &[bundle::sysroot::SharedSet]) -> Vec<String> {
    sets.iter().map(|set| set.name.clone()).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp directory unique per test and per call, so parallel tests
    /// never share state.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "onelf-assembly-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a recipe into `dir` and plan a build from it.
    fn plan_in(dir: &Path, body: &str) -> io::Result<AssemblyRequest> {
        let path = dir.join("onelf.toml");
        std::fs::write(&path, body)?;
        let recipe = recipe::load(&path)?;
        AssemblyRequest::plan(&path, recipe, None)
    }

    /// Whether a sysroot build pins the epoch mtime in this environment.
    fn expected_sysroot_mtime() -> Option<u64> {
        (pack::source_date_epoch().is_none()).then_some(0)
    }

    #[test]
    fn a_minimal_recipe_resolves_defaults() {
        let dir = temp_dir("defaults");
        let request = plan_in(&dir, "[package]\ncommand = \"bin/app\"\n").unwrap();
        assert_eq!(request.directory, dir);
        assert_eq!(request.output, dir.join("app.onelf"));
        assert_eq!(request.package.command, "bin/app");
        assert_eq!(request.package.name, None);
        assert_eq!(request.package.package_info, None);
        assert_eq!(request.package.mtime, None);
        assert_eq!(request.package.working_dir, WorkingDir::Inherit);
        assert_eq!(request.package.host_libs, pack::HostLibs::Auto);
        assert!(!request.package.cache && !request.package.prefer_cache);
        assert!(!request.package.needs_setuid);
        assert_eq!(request.package.memfd, None);
        assert_eq!(request.lib_dirs, ["auto"]);
        assert!(!request.bundle.skip);
        assert!(request.bundle.sysroot.is_none());
        assert!(request.bundle.sets.is_empty());
        assert!(request.bundle.platform_pin.is_none());
        assert_eq!(request.entrypoints, []);
        assert_eq!(request.default_entrypoint, None);
        assert_eq!(request.compression.payload, pack::Payload::Blocks);
        assert_eq!(request.compression.profile, None);
        assert!(!request.compression.use_dict && !request.compression.no_compress);
        assert_eq!(
            request.update,
            UpdateRequest {
                url: None,
                key_path: None,
                revision: None,
                unsigned: false,
                embed: true,
            }
        );
        assert!(request.env.is_empty() && request.paths.is_empty() && request.preload.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn output_paths_resolve_against_the_recipe_directory() {
        let dir = temp_dir("output");
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\noutput = \"out/pkg.onelf\"\n",
        )
        .unwrap();
        assert_eq!(request.output, dir.join("out/pkg.onelf"));
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/tool\"\noutput = \"/tmp/pkg.onelf\"\n",
        )
        .unwrap();
        assert_eq!(request.output, PathBuf::from("/tmp/pkg.onelf"));
        let path = dir.join("onelf.toml");
        let recipe = recipe::load(&path).unwrap();
        let request =
            AssemblyRequest::plan(&path, recipe, Some(Path::new("/tmp/override.onelf"))).unwrap();
        assert_eq!(request.output, PathBuf::from("/tmp/override.onelf"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn entrypoints_and_their_default_are_carried() {
        let dir = temp_dir("entrypoints");
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[[entrypoint]]\nname = \"cli\"\n\
             path = \"bin/app\"\nargs = [\"--cli\"]\n\n[[entrypoint]]\nname = \"gui\"\n\
             path = \"bin/app\"\ndefault = true\n",
        )
        .unwrap();
        assert_eq!(request.entrypoints.len(), 2);
        assert_eq!(request.entrypoints[0].args, ["--cli"]);
        assert_eq!(request.default_entrypoint.as_deref(), Some("gui"));
        let opts = request.pack_options(None, elf::RuntimeNote::Absent, Vec::new());
        assert_eq!(opts.default_entrypoint.as_deref(), Some("gui"));
        assert_eq!(
            opts.entrypoints,
            vec![
                (
                    "cli".to_string(),
                    "bin/app".to_string(),
                    vec!["--cli".to_string()]
                ),
                ("gui".to_string(), "bin/app".to_string(), Vec::new()),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn framework_flags_are_refused_with_a_sysroot_database() {
        let dir = temp_dir("conflict");
        let err = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[bundle]\ngl = true\nwayland = true\n\n\
             [sysroot]\npath = \"sysroot\"\n",
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("gl, wayland cannot be used with [sysroot]"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sysroot_database_overrides_skip_and_pins_the_epoch_mtime() {
        let sysroot = temp_dir("sysroot-db");
        let dir = temp_dir("sysroot-recipe");
        let body = format!(
            "[package]\ncommand = \"bin/app\"\n\n[bundle]\nskip = true\n\n\
             [sysroot]\npath = \"{}\"\n",
            sysroot.display()
        );
        let request = plan_in(&dir, &body).unwrap();
        assert!(!request.bundle.skip);
        assert!(request.bundle.sysroot.is_some());
        assert_eq!(request.package.mtime, expected_sysroot_mtime());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&sysroot).unwrap();
    }

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_set_only_sysroot_keeps_a_skipped_appdir_unchecked() {
        let dir = temp_dir("skip-sets");
        let body = format!(
            "[package]\ncommand = \"bin/app\"\n\n[bundle]\nskip = true\n\n\
             [sysroot.sets.gtk]\npackages = [\"gtk3\"]\nurl = \"https://e/gtk.onelf\"\n\
             blake3 = \"{HASH}\"\n"
        );
        let request = plan_in(&dir, &body).unwrap();
        assert!(request.bundle.skip);
        assert!(request.bundle.sysroot.is_none());
        assert_eq!(request.package.mtime, expected_sysroot_mtime());
        let report = request.stage().unwrap();
        assert_eq!(report.source, Source::Unchecked);
        assert_eq!(report.sets, ["gtk"]);
        assert!(report.frameworks.is_empty());
        assert!(!report.pinned_gl);
        assert!(dir.join(".onelf/sets").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_plain_skip_records_nothing() {
        let dir = temp_dir("skip-plain");
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[bundle]\nskip = true\n",
        )
        .unwrap();
        assert!(request.bundle.skip);
        let report = request.stage().unwrap();
        assert_eq!(
            report,
            AssemblyReport {
                source: Source::Unchecked,
                frameworks: Vec::new(),
                sets: Vec::new(),
                pinned_gl: false,
                active_rules: Vec::new(),
            }
        );
        assert!(!dir.join(".onelf").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_gl_pin_records_into_a_skipped_appdir() {
        let dir = temp_dir("skip-pin");
        let body = format!(
            "[package]\ncommand = \"bin/app\"\n\n[bundle]\nskip = true\n\n[sysroot]\n\
             platform-url = \"https://e/gl.onelf\"\nplatform-hash = \"{HASH}\"\n"
        );
        let request = plan_in(&dir, &body).unwrap();
        assert!(request.bundle.skip);
        assert!(request.bundle.platform_pin.is_some());
        let report = request.stage().unwrap();
        assert_eq!(report.source, Source::Unchecked);
        assert!(report.pinned_gl);
        assert!(dir.join(".onelf/platform").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn update_key_resolves_against_the_recipe_and_reads_bytes() {
        let dir = temp_dir("update-key");
        std::fs::write(dir.join("key.pub"), [7u8; 32]).unwrap();
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[update]\nurl = \"https://e/app.onelf\"\n\
             key = \"key.pub\"\nrevision = 3\n",
        )
        .unwrap();
        assert_eq!(
            request.update.key_path.as_deref(),
            Some(dir.join("key.pub").as_path())
        );
        assert_eq!(request.update.revision, Some(3));
        assert!(!request.update.unsigned);
        assert!(request.update.embed);
        assert_eq!(
            request.update.read_key().unwrap().as_deref(),
            Some(&[7u8; 32][..])
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_update_key_is_reported_with_its_recipe_context() {
        let dir = temp_dir("update-key-missing");
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[update]\nurl = \"https://e/app.onelf\"\n\
             key = \"absent.key\"\n",
        )
        .unwrap();
        let err = request.update.read_key().unwrap_err();
        assert!(err.to_string().contains("recipe [update] key"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn runtime_selection_follows_the_update_url_and_the_embed_flag() {
        let dir = temp_dir("runtime");
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[update]\nurl = \"https://e/app.onelf\"\n",
        )
        .unwrap();
        assert_eq!(
            embed_runtime(&request).unwrap().bytes,
            RUNTIME_BINARY_UPDATE
        );
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[update]\nurl = \"https://e/app.onelf\"\n\
             embed = false\n",
        )
        .unwrap();
        assert_eq!(embed_runtime(&request).unwrap().bytes, RUNTIME_BINARY_SLIM);
        let request = plan_in(&dir, "[package]\ncommand = \"bin/app\"\n").unwrap();
        assert_eq!(embed_runtime(&request).unwrap().bytes, RUNTIME_BINARY_SLIM);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A plain executable file under `base`, relative path `rel`,
    /// executable so a staged import of it is runnable.
    fn seed_binary(base: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = base.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    const UPSTREAM: &[u8] = b"#!/bin/sh\necho IMPORT_OK\n";

    #[test]
    fn an_import_resolves_against_the_recipe_directory() {
        let dir = temp_dir("import-resolve");
        seed_binary(&dir, "upstream/app", UPSTREAM);
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"upstream/app\"\n",
        )
        .unwrap();
        assert_eq!(
            request.input,
            Some(BinaryImport {
                source: dir.join("upstream/app"),
            })
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_binary_import_and_a_sysroot_database_are_refused_together() {
        let sysroot = temp_dir("import-sysroot");
        let dir = temp_dir("import-conflict");
        seed_binary(&dir, "upstream/app", UPSTREAM);
        std::fs::create_dir_all(&sysroot).unwrap();
        let body = format!(
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"upstream/app\"\n\n\
             [sysroot]\npath = \"{}\"\n",
            sysroot.display()
        );
        let err = plan_in(&dir, &body).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("[input] binary"), "{err}");
        assert!(text.contains("[sysroot] path"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&sysroot).unwrap();
    }

    #[test]
    fn a_binary_import_and_bundle_skip_are_refused_together() {
        let dir = temp_dir("import-skip");
        seed_binary(&dir, "upstream/app", UPSTREAM);
        let err = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"upstream/app\"\n\n\
             [bundle]\nskip = true\n",
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("[input] binary"), "{err}");
        assert!(text.contains("[bundle] skip"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_import_must_name_a_supported_file() {
        let dir = temp_dir("import-missing");
        let err = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"absent/app\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("bare executable"), "{err}");
        std::fs::create_dir_all(dir.join("tree")).unwrap();
        let err = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"tree\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("bare executable"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_import_refuses_a_command_that_leaves_the_appdir() {
        let dir = temp_dir("import-escape");
        seed_binary(&dir, "upstream/app", UPSTREAM);
        for command in ["../app", "/bin/app", "."] {
            let body = format!(
                "[package]\ncommand = \"{command}\"\n\n[input]\nbinary = \"upstream/app\"\n"
            );
            let err = plan_in(&dir, &body).unwrap_err();
            assert!(
                err.to_string().contains("plain relative path"),
                "{command}: {err}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_import_stages_the_command_and_records_it() {
        let dir = temp_dir("import-stage");
        seed_binary(&dir, "upstream/app", UPSTREAM);
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"upstream/app\"\n",
        )
        .unwrap();
        let report = request.stage().unwrap();
        assert_eq!(report.source, Source::HostScan);
        let dest = dir.join("bin/app");
        assert_eq!(std::fs::read(&dest).unwrap(), UPSTREAM);
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let entries = bundle::sysroot::read_generated(&dir).unwrap();
        let entry = entries
            .iter()
            .find(|e| e.path == "bin/app")
            .expect("recorded");
        assert_eq!(entry.hash, Some(hash_file(&dest).unwrap()));
        assert!(!entry.pending);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_import_refreshes_a_staged_copy_from_its_source() {
        let dir = temp_dir("import-refresh");
        let source = seed_binary(&dir, "upstream/app", UPSTREAM);
        let body = "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"upstream/app\"\n";
        let request = plan_in(&dir, body).unwrap();
        request.stage().unwrap();
        // An idempotent rebuild, a vanished staged copy, and a source
        // that changed upstream all refresh to the current source.
        request.stage().unwrap();
        std::fs::remove_file(dir.join("bin/app")).unwrap();
        request.stage().unwrap();
        std::fs::write(&source, b"#!/bin/sh\necho IMPORT_V2\n").unwrap();
        request.stage().unwrap();
        assert_eq!(
            std::fs::read(dir.join("bin/app")).unwrap(),
            b"#!/bin/sh\necho IMPORT_V2\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_edited_staged_import_stops_the_build() {
        let dir = temp_dir("import-edited");
        seed_binary(&dir, "upstream/app", UPSTREAM);
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"upstream/app\"\n",
        )
        .unwrap();
        request.stage().unwrap();
        std::fs::write(dir.join("bin/app"), b"#!/bin/sh\necho EDITED\n").unwrap();
        let err = request.stage().unwrap_err();
        assert!(err.to_string().contains("did not import"), "{err}");
        assert_eq!(
            std::fs::read(dir.join("bin/app")).unwrap(),
            b"#!/bin/sh\necho EDITED\n",
            "the edit must be preserved, not overwritten"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_import_refuses_a_destination_symlink_and_leaves_it_alone() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("import-link");
        seed_binary(&dir, "upstream/app", UPSTREAM);
        let victim = dir.join("victim");
        std::fs::write(&victim, b"do not touch\n").unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        symlink(&victim, dir.join("bin/app")).unwrap();
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"upstream/app\"\n",
        )
        .unwrap();
        let err = request.stage().unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch\n");
        assert!(
            std::fs::symlink_metadata(dir.join("bin/app"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_import_refuses_publisher_owned_content() {
        let dir = temp_dir("import-publisher");
        seed_binary(&dir, "upstream/app", UPSTREAM);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/app"), b"the publisher's own binary\n").unwrap();
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"upstream/app\"\n",
        )
        .unwrap();
        let err = request.stage().unwrap_err();
        assert!(err.to_string().contains("did not import"), "{err}");
        assert_eq!(
            std::fs::read(dir.join("bin/app")).unwrap(),
            b"the publisher's own binary\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_import_through_a_linked_directory_is_refused() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("import-linkdir");
        seed_binary(&dir, "upstream/app", UPSTREAM);
        let outside = dir.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, dir.join("bin")).unwrap();
        let request = plan_in(
            &dir,
            "[package]\ncommand = \"bin/app\"\n\n[input]\nbinary = \"upstream/app\"\n",
        )
        .unwrap();
        let err = request.stage().unwrap_err();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(!outside.join("app").exists(), "nothing may land outside");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }
}
