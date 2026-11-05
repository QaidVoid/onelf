//! Typed framework descriptors and filesystem-independent validation.
//!
//! A descriptor has an explicit schema and identity. Trigger predicates are ORed
//! over parsed ELF sonames. Resource and runtime candidate order is significant.
//! Resource destinations are package-relative and directory copies preserve the
//! complete candidate-relative layout. A plugin group may list `members`, paths
//! relative to the found directory, to collect only those members instead of
//! every compatible one. Helpers name an associated plugin or data group in the
//! same descriptor. Resource IDs share one namespace, while runtime
//! path IDs have their own namespace, both scoped to the descriptor.
//!
//! TOML parsing is available only with the `rules-toml` feature through
//! `rules_toml`. Tables use kebab-case field names and reject unknown
//! fields. Collections default to empty, even inside explicitly present tables.
//! Required resource fields and path policies have no implicit defaults. Hook
//! names are fixed snake_case registry IDs, not executable commands.
//!
//! A file resource with ordered alternatives and a caller-preserving mapping:
//!
//! ```toml
//! schema = 1
//! id = "magic"
//!
//! [[triggers]]
//! soname-prefix = "libmagic.so"
//!
//! [[resources.data]]
//! id = "database"
//! kind = "file"
//! destination = "share/misc/magic.mgc"
//! requirement = "required"
//! candidates = [
//!   { base = "installation-prefix", path = "share/file/misc/magic.mgc" },
//!   { base = "installation-prefix", path = "share/misc/magic.mgc" },
//! ]
//!
//! [[runtime.paths]]
//! id = "database"
//! kind = "file"
//! variables = ["MAGIC"]
//! selection = "first"
//! inheritance = "preserve"
//! candidates = [
//!   { base = "package-root", path = "share/file/misc/magic.mgc" },
//!   { base = "package-root", path = "share/misc/magic.mgc" },
//! ]
//! ```
//!
//! Validation is lexical only. Consumers must additionally check actual resource
//! kinds, admitted source roots, symlink confinement and destination traversal
//! before any filesystem operation. These definitions do not discover resources,
//! run hooks, evaluate runtime mappings or provide a default catalog.

use std::collections::BTreeSet;
use std::fmt;

/// Supported descriptor schema version.
pub const SCHEMA_VERSION: u16 = 1;
/// Maximum UTF-8 TOML input size in bytes, checked before parsing.
pub const MAX_DESCRIPTOR_BYTES: usize = 1024 * 1024;
/// Maximum aggregate records in a descriptor or validated catalog.
///
/// Each descriptor, trigger, resource group, candidate, plugin member, helper
/// group reference, runtime path group, variable alias, hook and disabled ID
/// counts as one record.
/// Container tables do not count as records.
pub const MAX_RECORDS: usize = 4096;
/// Maximum bytes in a rule/group ID or environment variable name.
pub const MAX_ID_BYTES: usize = 128;
/// Maximum bytes in a path or soname prefix, before path normalization.
pub const MAX_PATH_BYTES: usize = 4096;

/// A bounded, actionable descriptor validation or parsing error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError(String);

impl RuleError {
    /// Build a bounded error for a named descriptor field.
    pub fn new(field: &str, reason: &str) -> Self {
        let reason: String = reason.chars().take(512).collect();
        Self(format!("{field}: {reason}"))
    }

    fn value(field: &str, reason: &str, value: &str) -> Self {
        let excerpt: String = value
            .chars()
            .take(MAX_ID_BYTES)
            .flat_map(char::escape_default)
            .collect();
        Self::new(field, &format!("{reason}: {excerpt}"))
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RuleError {}

/// A nonempty stable rule or descriptor-local group identity.
///
/// IDs contain only ASCII letters, digits, dots, underscores and hyphens, with
/// an alphanumeric first byte. Identity matching is case-sensitive.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuleId(String);

impl RuleId {
    /// Validate an identity of at most [`MAX_ID_BYTES`] bytes.
    pub fn new(value: &str) -> Result<Self, RuleError> {
        if value.len() > MAX_ID_BYTES {
            return Err(RuleError::new("id", "exceeds MAX_ID_BYTES (128)"));
        }
        if !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(RuleError::value(
                "id",
                "expected an alphanumeric identity",
                value,
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// Validate a set declaration name: an identity without dots.
    ///
    /// Provider indexes and the selection and pin tables apply this one rule,
    /// so a set name one of them accepts is never refused by another.
    pub fn set_name(value: &str) -> Result<Self, RuleError> {
        let id = Self::new(value)?;
        if value.contains('.') {
            return Err(RuleError::value(
                "set name",
                "expected ASCII letters, digits, - and _ only",
                value,
            ));
        }
        Ok(id)
    }

    /// The validated identity text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A normalized, nonempty path relative to a typed source or runtime base.
///
/// Redundant slashes and `.` components are removed. Parent components, absolute
/// paths, Windows drive prefixes, backslashes and control bytes are rejected.
/// Normalization does not consult the filesystem or follow symlinks.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelativePath(String);

impl RelativePath {
    /// Validate and normalize a relative path of at most [`MAX_PATH_BYTES`] bytes.
    pub fn new(value: &str) -> Result<Self, RuleError> {
        if value.len() > MAX_PATH_BYTES {
            return Err(RuleError::new("path", "exceeds MAX_PATH_BYTES (4096)"));
        }
        let first_component = value
            .split('/')
            .find(|part| !part.is_empty() && *part != ".")
            .unwrap_or_default();
        let windows_drive = first_component.as_bytes().get(1) == Some(&b':')
            && first_component
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic);
        if value.starts_with('/')
            || windows_drive
            || value.contains('\\')
            || value.bytes().any(|b| b.is_ascii_control())
            || value.split('/').any(|part| part == "..")
        {
            return Err(RuleError::value(
                "path",
                "expected a confined relative path",
                value,
            ));
        }
        let normalized = value
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect::<Vec<_>>()
            .join("/");
        if normalized.is_empty() {
            return Err(RuleError::new(
                "path",
                "must name a resource below its base",
            ));
        }
        Ok(Self(normalized))
    }

    /// The normalized relative path using `/` separators.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A portable environment name that cannot configure the application loader.
///
/// Names use `[A-Za-z_][A-Za-z0-9_]*`. Every `LD_` name is prohibited because
/// the dynamic loader reads that namespace, including `LD_LIBRARY_PATH`,
/// `LD_PRELOAD` and `LD_AUDIT`. `GCONV_PATH` and `GLIBC_TUNABLES`, which make
/// glibc load code or change loader behavior, and every runtime-owned `ONELF_`
/// name are prohibited too. Framework-specific plugin variables remain
/// permitted because they do not provide ordinary ELF dependencies.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvironmentName(String);

impl EnvironmentName {
    /// Validate an environment name of at most [`MAX_ID_BYTES`] bytes.
    pub fn new(value: &str) -> Result<Self, RuleError> {
        if value.len() > MAX_ID_BYTES {
            return Err(RuleError::new("environment", "exceeds MAX_ID_BYTES (128)"));
        }
        if !value
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(RuleError::value(
                "environment",
                "invalid variable name",
                value,
            ));
        }
        if value.starts_with("LD_") || matches!(value, "GCONV_PATH" | "GLIBC_TUNABLES") {
            return Err(RuleError::value(
                "environment",
                "prohibited library-search or loader-control variable",
                value,
            ));
        }
        if value.starts_with("ONELF_") {
            return Err(RuleError::value(
                "environment",
                "prohibited runtime-owned variable",
                value,
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// The validated environment name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How a library reads one runtime-assigned environment name's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueShape {
    /// One path naming a single file or directory.
    Single,
    /// A colon-separated list of paths.
    List,
}

/// One environment name a closed list assigns, with how a library reads
/// its value.
pub(crate) type NameShape = (&'static str, ValueShape, ResourceKind);

/// The environment names of one closed list.
pub(crate) type AssignedNames = &'static [NameShape];

/// The environment names each registered runtime hook assigns, with how
/// a library reads each value. The hook registry is closed, so this is
/// a fixed list.
pub(crate) const HOOK_NAMES: &[(RuntimeHookId, AssignedNames)] = &[
    (
        RuntimeHookId::AlsaConfigPathNaturalSort,
        &[("ALSA_CONFIG_PATH", ValueShape::List, ResourceKind::File)],
    ),
    (
        RuntimeHookId::GdkCacheAbsolutise,
        &[
            (
                "GDK_PIXBUF_MODULE_FILE",
                ValueShape::Single,
                ResourceKind::File,
            ),
            ("GTK_IM_MODULE_FILE", ValueShape::Single, ResourceKind::File),
        ],
    ),
    (
        RuntimeHookId::FontconfigGenerate,
        &[("FONTCONFIG_FILE", ValueShape::Single, ResourceKind::File)],
    ),
    (
        RuntimeHookId::GioModulePaths,
        &[
            (
                "GIO_MODULE_DIR",
                ValueShape::Single,
                ResourceKind::Directory,
            ),
            (
                "GIO_EXTRA_MODULES",
                ValueShape::List,
                ResourceKind::Directory,
            ),
        ],
    ),
    (
        RuntimeHookId::TclTkLibraries,
        &[
            ("TCL_LIBRARY", ValueShape::Single, ResourceKind::Directory),
            ("TK_LIBRARY", ValueShape::Single, ResourceKind::Directory),
        ],
    ),
];

/// The environment names the launch's own procedures assign, with how a
/// library reads each value: the loader's charset converters, the
/// graphics driver paths, the terminal descriptions, the trust anchors
/// and the locales.
pub(crate) const PROCEDURE_NAMES: AssignedNames = &[
    ("GCONV_PATH", ValueShape::List, ResourceKind::Directory),
    (
        "LIBDECOR_PLUGIN_DIR",
        ValueShape::Single,
        ResourceKind::Directory,
    ),
    (
        "LIBGL_DRIVERS_PATH",
        ValueShape::List,
        ResourceKind::Directory,
    ),
    (
        "LIBVA_DRIVERS_PATH",
        ValueShape::List,
        ResourceKind::Directory,
    ),
    (
        "GBM_BACKENDS_PATH",
        ValueShape::List,
        ResourceKind::Directory,
    ),
    (
        "__EGL_VENDOR_LIBRARY_DIRS",
        ValueShape::List,
        ResourceKind::Directory,
    ),
    ("VK_DRIVER_FILES", ValueShape::List, ResourceKind::File),
    ("VK_ADD_DRIVER_FILES", ValueShape::List, ResourceKind::File),
    (
        "DRIRC_CONFIGDIR",
        ValueShape::Single,
        ResourceKind::Directory,
    ),
    (
        "LIBDRM_IDS_PATH",
        ValueShape::Single,
        ResourceKind::Directory,
    ),
    ("TERMINFO_DIRS", ValueShape::List, ResourceKind::Directory),
    ("SSL_CERT_FILE", ValueShape::Single, ResourceKind::File),
    ("CURL_CA_BUNDLE", ValueShape::Single, ResourceKind::File),
    ("LOCPATH", ValueShape::List, ResourceKind::Directory),
];

/// Whether a registered runtime hook or the launch's own procedures
/// assign `name`, so no descriptor may map it.
fn runtime_owned(name: &str) -> bool {
    HOOK_NAMES
        .iter()
        .flat_map(|(_, names)| names.iter())
        .chain(PROCEDURE_NAMES)
        .any(|(owned, _, _)| *owned == name)
}

/// A case-sensitive predicate over the complete parsed ELF soname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    /// Literal soname prefix, without path components or glob syntax.
    pub soname_prefix: String,
}

impl Trigger {
    /// Match a parsed soname, not arbitrary binary bytes or filesystem paths.
    ///
    /// Empty values and paths never match. Prefixes cannot contain pattern
    /// syntax, but literal metacharacters in a candidate suffix are permitted.
    pub fn matches_soname(&self, soname: &str) -> bool {
        valid_prefix(&self.soname_prefix)
            && valid_soname(soname)
            && soname.starts_with(&self.soname_prefix)
    }
}

fn valid_prefix(value: &str) -> bool {
    valid_soname(value) && !value.bytes().any(|b| b":*?[]".contains(&b))
}

fn valid_soname(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PATH_BYTES
        && value != "."
        && value != ".."
        && !value
            .bytes()
            .any(|b| b.is_ascii_control() || b"/\\".contains(&b))
}

/// The kind a resource must have at discovery or runtime evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// A regular file, including a confined symlink resolving to one.
    File,
    /// A directory, including a confined symlink resolving to one.
    Directory,
}

/// Whether absence is an assembly failure or an optional-group finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// The activated group must be available from the eligible source.
    Required,
    /// Unavailable automatic backends may be omitted with a finding.
    Optional,
}

/// An admitted source location, never an arbitrary host path expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceBase {
    /// The admitted installation prefix of a resolved framework library.
    InstallationPrefix,
    /// An admitted library directory belonging to that installation.
    LibraryDir,
}

/// One ordered source alternative relative to an admitted source base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCandidate {
    /// The admitted location to search beneath.
    pub base: SourceBase,
    /// Normalized path below that location.
    pub path: RelativePath,
}

/// A plugin tree or data resource with ordered source alternatives.
///
/// Plugin groups require directory kind and select the complete compatible ELF
/// tree, or only the listed members of it. Data groups may require a file or a
/// directory and list no members. Source alternatives are tried in order,
/// without broadening the admitted source or package universe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceGroup {
    /// Descriptor-local resource identity.
    pub id: RuleId,
    /// Required source and destination kind.
    pub kind: ResourceKind,
    /// Ordered alternative locations. A declared group requires at least one.
    pub candidates: Vec<SourceCandidate>,
    /// Package-relative destination. Directory contents retain their layout.
    pub destination: RelativePath,
    /// Treatment of unavailable resources after source policy is applied.
    pub requirement: Requirement,
    /// Plugin group member allowlist, as paths relative to the found
    /// directory. Empty selects every compatible member. A listed member the
    /// source lacks is not an error. The requirement applies to what remains.
    pub members: Vec<RelativePath>,
}

/// A framework helper executable associated with a plugin or data group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperGroup {
    /// Descriptor-local resource identity, distinct from other resources.
    pub id: RuleId,
    /// Associated plugin or data group in the same descriptor.
    pub group: RuleId,
    /// Must be [`ResourceKind::File`].
    pub kind: ResourceKind,
    /// Ordered executable file alternatives. At least one is required.
    pub candidates: Vec<SourceCandidate>,
    /// Package-relative executable destination.
    pub destination: RelativePath,
    /// Treatment of an unavailable helper.
    pub requirement: Requirement,
}

/// Discovery resources. Every omitted collection is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    /// Compatible ELF plugin trees, retaining group-relative layouts.
    pub plugins: Vec<ResourceGroup>,
    /// Required or optional data files and directory trees.
    pub data: Vec<ResourceGroup>,
    /// Helper executable alternatives associated with plugin or data groups.
    pub helpers: Vec<HelperGroup>,
}

/// A runtime base evaluated within the application and canonical active roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeBase {
    /// A package root, with the application before selected sets.
    PackageRoot,
    /// Each library root belonging to the application or selected sets.
    LibraryRoot,
}

/// One ordered runtime path candidate. It never names a build-host source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCandidate {
    /// Package or library root under which the path is evaluated.
    pub base: RuntimeBase,
    /// Normalized relative location within that base.
    pub path: RelativePath,
}

/// Selection of existing candidates of the declared kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSelection {
    /// First matching candidate, with application roots before selected sets.
    First,
    /// All matching paths in root and candidate order, without duplicates.
    All,
    /// First matching alternative per library root, such as nested heif plugins.
    FirstPerLibraryRoot,
}

/// Policy for a caller-supplied environment value when a path group is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inheritance {
    /// Keep a present caller value, including an explicitly empty one.
    Preserve,
    /// Replace an inherited value with the matching carried resource paths.
    Replace,
}

/// A presence-based runtime environment mapping with one value for all aliases.
///
/// These groups do not require runtime soname activation. With no matching
/// candidate, no variable is assigned. Specialized multi-variable behavior such
/// as GIO's primary and extra module paths belongs to a registered runtime hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePathGroup {
    /// Identity within the descriptor's runtime path namespace.
    pub id: RuleId,
    /// File or directory kind to test before selecting a candidate.
    pub kind: ResourceKind,
    /// Ordered path alternatives. At least one is required.
    pub candidates: Vec<RuntimeCandidate>,
    /// Environment aliases receiving the same value. At least one is required.
    pub variables: Vec<EnvironmentName>,
    /// First, all or per-library-root candidate selection.
    pub selection: PathSelection,
    /// Preserve or replace caller-supplied values when a resource is present.
    pub inheritance: Inheritance,
}

/// Closed build-time hook registry. These procedures modify staged resources
/// before hashing and cannot be requested as runtime hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BuildHookId {
    /// Anchor carried ALSA snippets in the staged `alsa.conf` hook list.
    AlsaConfPatch,
}

/// Closed runtime hook registry. Handlers produce private writable output and
/// must never patch verified payload bytes or execute descriptor-supplied code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeHookId {
    /// Select ALSA configuration and naturally ordered snippets while preserving
    /// explicit user configuration and rejecting unrepresentable path lists.
    AlsaConfigPathNaturalSort,
    /// Absolutise GDK pixbuf and GTK input-method cache module paths privately.
    GdkCacheAbsolutise,
    /// Generate private fontconfig output with caller configuration precedence.
    FontconfigGenerate,
    /// Assign GIO's first module directory and remaining extra directories.
    GioModulePaths,
    /// Find matching versioned Tcl and Tk script libraries in supported layouts.
    TclTkLibraries,
}

/// Build-phase procedures. An omitted hook list is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildRules {
    /// Ordered registered build-time procedures, without duplicates.
    pub hooks: Vec<BuildHookId>,
}

/// Runtime paths and procedures. Omitted collections are empty independently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeRules {
    /// Ordered presence-based environment path groups.
    pub paths: Vec<RuntimePathGroup>,
    /// Ordered registered runtime procedures, without duplicates.
    pub hooks: Vec<RuntimeHookId>,
}

/// A versioned framework descriptor with optional discovery and runtime behavior.
///
/// Direct construction must be followed by [`Self::validate`]. The feature-gated
/// TOML parser performs that validation before returning a descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// Must equal [`SCHEMA_VERSION`]. This field is required in TOML.
    pub schema: u16,
    /// Stable identity, unique across built-in and selected local descriptors.
    pub id: RuleId,
    /// ORed predicates used during build-time ELF closure discovery.
    pub triggers: Vec<Trigger>,
    /// Discovery groups. Missing tables and collections are empty.
    pub resources: Resources,
    /// Build procedures. Missing tables and hook lists are empty.
    pub build: BuildRules,
    /// Presence-based runtime paths and procedures, independent of triggers.
    pub runtime: RuntimeRules,
}

impl Descriptor {
    /// Check schema, record bounds, identities, kinds, policies and references.
    ///
    /// The record bound is checked before allocating validation indexes. Paths,
    /// environment names and IDs are validated by their constructors. Filesystem
    /// confinement and actual on-disk kinds remain the caller's responsibility.
    pub fn validate(&self) -> Result<(), RuleError> {
        self.count_records(&mut RecordBudget::default())?;
        self.validate_fields()
    }

    fn count_records(&self, budget: &mut RecordBudget) -> Result<(), RuleError> {
        budget.add(1)?;
        budget.add(self.triggers.len())?;
        budget.add(self.resources.plugins.len())?;
        budget.add(self.resources.data.len())?;
        budget.add(self.resources.helpers.len())?;
        budget.add(self.runtime.paths.len())?;
        budget.add(self.build.hooks.len())?;
        budget.add(self.runtime.hooks.len())?;
        for resource in self.resources.plugins.iter().chain(&self.resources.data) {
            budget.add(resource.candidates.len())?;
            budget.add(resource.members.len())?;
        }
        for helper in &self.resources.helpers {
            budget.add(1)?;
            budget.add(helper.candidates.len())?;
        }
        for path in &self.runtime.paths {
            budget.add(path.candidates.len())?;
            budget.add(path.variables.len())?;
        }
        Ok(())
    }

    fn validate_fields(&self) -> Result<(), RuleError> {
        if self.schema != SCHEMA_VERSION {
            return Err(RuleError::new(
                "schema",
                &format!(
                    "unsupported version {} for {}",
                    self.schema,
                    self.id.as_str()
                ),
            ));
        }
        let mut triggers = BTreeSet::new();
        for trigger in &self.triggers {
            if !valid_prefix(&trigger.soname_prefix) {
                return Err(RuleError::value(
                    "soname-prefix",
                    "expected a literal soname prefix",
                    &trigger.soname_prefix,
                ));
            }
            unique(
                &mut triggers,
                trigger.soname_prefix.as_str(),
                "soname-prefix",
            )?;
        }
        let mut groups = BTreeSet::new();
        for resource in self.resources.plugins.iter().chain(&self.resources.data) {
            unique(&mut groups, &resource.id, "resource id")?;
            require_candidates(&resource.candidates, resource.id.as_str())?;
        }
        for plugin in &self.resources.plugins {
            if plugin.kind != ResourceKind::Directory {
                return Err(RuleError::value(
                    "plugins.kind",
                    "must be directory",
                    plugin.id.as_str(),
                ));
            }
            let mut members = BTreeSet::new();
            for member in &plugin.members {
                unique(&mut members, member, "plugins.members")?;
            }
        }
        if let Some(data) = self.resources.data.iter().find(|d| !d.members.is_empty()) {
            return Err(RuleError::value(
                "data.members",
                "only plugin groups list members",
                data.id.as_str(),
            ));
        }
        for helper in &self.resources.helpers {
            if !groups.contains(&helper.group) {
                return Err(RuleError::value(
                    "helpers.group",
                    "unknown plugin or data group",
                    helper.group.as_str(),
                ));
            }
            if helper.kind != ResourceKind::File {
                return Err(RuleError::value(
                    "helpers.kind",
                    "must be file",
                    helper.id.as_str(),
                ));
            }
            require_candidates(&helper.candidates, helper.id.as_str())?;
        }
        for helper in &self.resources.helpers {
            unique(&mut groups, &helper.id, "resource id")?;
        }
        self.runtime.validate_fields()?;
        let mut build_hooks = BTreeSet::new();
        for hook in &self.build.hooks {
            unique(&mut build_hooks, hook, "build hook")?;
        }
        Ok(())
    }
}

impl RuntimeRules {
    /// Check record bounds, identities, aliases, policies and hook dedup.
    ///
    /// Validation is lexical and filesystem-independent. The record bound is
    /// checked before allocating validation indexes.
    pub fn validate(&self) -> Result<(), RuleError> {
        let mut budget = RecordBudget::default();
        budget.add(self.paths.len())?;
        budget.add(self.hooks.len())?;
        for path in &self.paths {
            budget.add(path.candidates.len())?;
            budget.add(path.variables.len())?;
        }
        self.validate_fields()
    }

    fn validate_fields(&self) -> Result<(), RuleError> {
        let mut paths = BTreeSet::new();
        for path in &self.paths {
            unique(&mut paths, &path.id, "runtime path id")?;
            require_candidates(&path.candidates, path.id.as_str())?;
            if path.variables.is_empty() {
                return Err(RuleError::value(
                    "runtime.paths.variables",
                    "requires at least one environment alias",
                    path.id.as_str(),
                ));
            }
            let mut variables = BTreeSet::new();
            for variable in &path.variables {
                if runtime_owned(variable.as_str()) {
                    return Err(RuleError::value(
                        "runtime.paths.variables",
                        "assigned by a runtime hook or launch procedure",
                        variable.as_str(),
                    ));
                }
                unique(&mut variables, variable, "environment alias")?;
            }
            if path.selection == PathSelection::FirstPerLibraryRoot
                && path
                    .candidates
                    .iter()
                    .any(|c| c.base != RuntimeBase::LibraryRoot)
            {
                return Err(RuleError::value(
                    "runtime.paths.selection",
                    "first-per-library-root requires library-root candidates",
                    path.id.as_str(),
                ));
            }
        }
        let mut runtime_hooks = BTreeSet::new();
        for hook in &self.hooks {
            unique(&mut runtime_hooks, hook, "runtime hook")?;
        }
        Ok(())
    }
}

/// What an inference exclusion entry is relative to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExclusionBase {
    /// The installation prefix of the object that compiles the
    /// candidate in.
    InstallationPrefix,
    /// The library directories of that prefix. The entry names its path
    /// joined to the innermost library directory a candidate lies in,
    /// `lib/x86_64-linux-gnu` rather than `lib` for a path below the
    /// former, never a same-named directory deeper down. Its first
    /// component cannot have the multiarch shape, since that would name
    /// a library directory itself.
    LibraryDirs,
}

/// One directory inference never collects, relative to its base.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExclusionEntry {
    /// What the path is relative to.
    pub base: ExclusionBase,
    /// Normalized path below that base.
    pub path: RelativePath,
}

/// The directories compiled-in path inference never collects.
///
/// A bundled library names many absolute directories, and only those
/// specific to one framework belong in a package. The library
/// directories of an installation prefix are its `lib`, `lib32`, `lib64`
/// and `libx32`, and each directory directly below one of them whose name
/// has the multiarch shape `<arch>-linux-<abi>`, such as
/// `x86_64-linux-gnu`. The directory an object sits in is a library
/// directory only when it is one of these, since a framework can keep
/// plugins beside its own binaries, as Xorg does in `lib/xorg/modules`.
///
/// There are two matching modes.
///
/// - A shared root, relative to the prefix, holds several packages'
///   directories side by side while framework directories live below it,
///   so a candidate that equals or contains one is dropped.
/// - Every other entry, a shared tree, a libc directory or a driver
///   directory, is dropped together with what lies below it: a candidate
///   that equals, contains or lies below one is dropped. Shared trees
///   hold every package's files, or the host system's own, at every
///   depth. The libc table names directories the libc handling carries
///   or leaves to the host, and the driver table the driver stacks, which
///   come from the host unless the publisher bundles them.
///
/// Three rules are structural and hold whatever the tables say.
///
/// - A candidate must lie strictly below a resource base: a library
///   directory or the prefix's `share`. Frameworks keep their plugins and
///   data there, and the rest of a prefix is host configuration, build
///   input or system state. A base itself, and anything containing one,
///   is never a candidate.
/// - A candidate that equals or contains the directory the object sits
///   in is dropped, since that directory holds the object itself, and
///   every package's libraries when it is a library directory.
/// - When the prefix is `/`, the directory the object sits in, the library
///   directories, the bases and every entry of these tables also stand
///   below `usr/`, because `/` and `/usr` are one hierarchy on merged and
///   unmerged hosts alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceExclusions {
    /// Shared roots relative to the prefix. A candidate that equals or
    /// contains one is dropped.
    pub shared_roots: Vec<RelativePath>,
    /// Shared trees. A candidate that equals, contains or lies below one
    /// is dropped.
    pub shared_trees: Vec<ExclusionEntry>,
    /// Directories owned by the libc family. A candidate that equals,
    /// contains or lies below one is dropped.
    pub libc: Vec<ExclusionEntry>,
    /// Directories owned by the driver stacks. A candidate that equals,
    /// contains or lies below one is dropped.
    pub drivers: Vec<ExclusionEntry>,
}

impl InferenceExclusions {
    /// Check the record bound, that some shared root is listed, that no
    /// root is listed twice or also names a prefix-relative entry of
    /// another table, that no entry is listed twice across the tree, libc
    /// and driver tables, and that no lib-dirs entry begins with a
    /// multiarch directory.
    pub fn validate(&self) -> Result<(), RuleError> {
        let mut budget = RecordBudget::default();
        budget.add(self.shared_roots.len())?;
        budget.add(self.shared_trees.len())?;
        budget.add(self.libc.len())?;
        budget.add(self.drivers.len())?;
        if self.shared_roots.is_empty() {
            return Err(RuleError::new(
                "shared-roots",
                "must name at least one root",
            ));
        }
        let mut roots = BTreeSet::new();
        for root in &self.shared_roots {
            unique(&mut roots, root, "shared root")?;
        }
        let mut owned = BTreeSet::new();
        for entry in self
            .shared_trees
            .iter()
            .chain(&self.libc)
            .chain(&self.drivers)
        {
            if entry.base == ExclusionBase::InstallationPrefix && roots.contains(&entry.path) {
                return Err(RuleError::value(
                    "shared-roots",
                    "also names a tree, libc or driver directory",
                    entry.path.as_str(),
                ));
            }
            let first = entry.path.as_str().split('/').next().unwrap_or_default();
            if entry.base == ExclusionBase::LibraryDirs && is_multiarch(first) {
                return Err(RuleError::value(
                    "lib-dirs entry",
                    "begins with a multiarch library directory",
                    entry.path.as_str(),
                ));
            }
            unique(&mut owned, entry, "shared tree, libc or driver directory")?;
        }
        Ok(())
    }
}

/// Whether a directory name has the multiarch shape `<arch>-linux-<abi>`,
/// such as `x86_64-linux-gnu` or `arm-linux-gnueabihf`: the name of a
/// distribution's per-target library directory below `lib`. This is the
/// one test of that shape, shared by the exclusion table's validation and
/// the inference that matches against it.
pub fn is_multiarch(name: &str) -> bool {
    let mut parts = name.splitn(3, '-');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(arch), Some("linux"), Some(abi)) if !arch.is_empty() && !abi.is_empty()
    )
}

#[derive(Default)]
struct RecordBudget(usize);

impl RecordBudget {
    fn add(&mut self, count: usize) -> Result<(), RuleError> {
        self.0 = self
            .0
            .checked_add(count)
            .filter(|n| *n <= MAX_RECORDS)
            .ok_or_else(|| RuleError::new("records", "exceeds MAX_RECORDS (4096)"))?;
        Ok(())
    }
}

fn unique<T: Ord + fmt::Debug>(
    set: &mut BTreeSet<T>,
    value: T,
    field: &str,
) -> Result<(), RuleError> {
    if set.contains(&value) {
        return Err(RuleError::new(
            field,
            &format!("duplicate identity {value:?}"),
        ));
    }
    set.insert(value);
    Ok(())
}

fn require_candidates<T>(candidates: &[T], id: &str) -> Result<(), RuleError> {
    if candidates.is_empty() {
        return Err(RuleError::value(
            "candidates",
            "requires at least one source or runtime candidate",
            id,
        ));
    }
    Ok(())
}

/// Validate a complete catalog and its explicit disabled identities.
///
/// Built-ins and local rules share an identity namespace even when disabled.
/// Unknown and repeated disabled IDs are errors. An environment name may be
/// mapped by only one runtime path group across the enabled descriptors, so
/// every consumer classifies it the same way. Aggregate records are bounded
/// before indexing. This function neither sorts nor filters its inputs, and does
/// not load files or define any built-in descriptors.
pub fn validate_catalog(
    builtins: &[Descriptor],
    local: &[Descriptor],
    disabled: &[RuleId],
) -> Result<(), RuleError> {
    let mut budget = RecordBudget::default();
    budget.add(disabled.len())?;
    for rule in builtins.iter().chain(local) {
        rule.count_records(&mut budget)?;
    }
    let mut ids = BTreeSet::new();
    for rule in builtins.iter().chain(local) {
        rule.validate_fields()?;
        unique(&mut ids, &rule.id, "rule id")?;
    }
    let mut disabled_ids = BTreeSet::new();
    for id in disabled {
        if !ids.contains(id) {
            return Err(RuleError::value(
                "disable-rules",
                "unknown rule id",
                id.as_str(),
            ));
        }
        unique(&mut disabled_ids, id, "disabled rule id")?;
    }
    let mut mapped = BTreeSet::new();
    for rule in builtins.iter().chain(local) {
        if disabled_ids.contains(&rule.id) {
            continue;
        }
        for variable in rule.runtime.paths.iter().flat_map(|path| &path.variables) {
            if !mapped.insert(variable) {
                return Err(RuleError::value(
                    "runtime.paths.variables",
                    "mapped by more than one enabled runtime path group",
                    variable.as_str(),
                ));
            }
        }
    }
    Ok(())
}
