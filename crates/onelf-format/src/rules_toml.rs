//! CLI-only bounded TOML parsing for typed framework descriptors.
//!
//! The schema has at most four structural levels: descriptor, resource/runtime
//! table, group and candidate. Unknown nested fields and operations are errors.
//! No parser or serde dependency is enabled by the format crate's default
//! features. The adapter constructs shared types explicitly, so a build script
//! can include this module and `rules` with only its own TOML build dependency.

use crate::rules::*;
use toml::Value;

/// Parse one descriptor from at most 1 MiB of UTF-8 TOML and validate it.
///
/// Omitted collections default to empty. Declared groups still require usable
/// candidates, and runtime mappings require environment aliases. Parsing and
/// validation finish before the caller can use any portion of the descriptor.
/// Parse diagnostics include a location but never copy an unbounded source line.
pub fn parse_descriptor(text: &str) -> Result<Descriptor, RuleError> {
    if text.len() > MAX_DESCRIPTOR_BYTES {
        return Err(RuleError::new(
            "TOML",
            "exceeds MAX_DESCRIPTOR_BYTES (1048576)",
        ));
    }
    let value = parse_document(text)?;
    let mut fields = Fields::new(
        value,
        "descriptor",
        &["schema", "id", "triggers", "resources", "build", "runtime"],
    )?;
    let schema = match fields.required("schema")? {
        Value::Integer(version) => u16::try_from(version)
            .map_err(|_| RuleError::new("schema", "expected an unsigned 16-bit version"))?,
        _ => return Err(RuleError::new("schema", "expected an integer version")),
    };
    let descriptor = Descriptor {
        schema,
        id: RuleId::new(&fields.string("id")?)?,
        triggers: fields.list("triggers", parse_trigger)?,
        resources: fields.section("resources", parse_resources)?,
        build: fields.section("build", parse_build)?,
        runtime: fields.section("runtime", parse_runtime)?,
    };
    descriptor.validate()?;
    Ok(descriptor)
}

/// Parse the inference exclusions from at most 1 MiB of UTF-8 TOML and
/// validate them. Every table is a top-level field: `shared-roots`, a list
/// of prefix-relative paths, and `shared-trees`, `libc` and `drivers`,
/// lists of entries whose base is `installation-prefix` or `lib-dirs`.
pub fn parse_inference_exclusions(text: &str) -> Result<InferenceExclusions, RuleError> {
    if text.len() > MAX_DESCRIPTOR_BYTES {
        return Err(RuleError::new(
            "TOML",
            "exceeds the 1048576 byte limit on TOML input",
        ));
    }
    let value = parse_document(text)?;
    let mut fields = Fields::new(
        value,
        "inference exclusions",
        &["shared-roots", "shared-trees", "libc", "drivers"],
    )?;
    let exclusions = InferenceExclusions {
        shared_roots: fields.list("shared-roots", parse_shared_root)?,
        shared_trees: fields.list("shared-trees", parse_exclusion_entry)?,
        libc: fields.list("libc", parse_exclusion_entry)?,
        drivers: fields.list("drivers", parse_exclusion_entry)?,
    };
    exclusions.validate()?;
    Ok(exclusions)
}

/// Parse TOML the caller has already bounded, with a parse error located
/// by line and column and never copying an unbounded source line.
fn parse_document(text: &str) -> Result<Value, RuleError> {
    text.parse::<toml::Table>()
        .map(Value::Table)
        .map_err(|error| {
            let message: String = error.message().chars().take(512).collect();
            let offset = error.span().map_or(0, |span| span.start.min(text.len()));
            let preceding = &text.as_bytes()[..offset];
            let line = preceding.iter().filter(|b| **b == b'\n').count() + 1;
            let column = preceding
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(offset + 1, |last| offset - last);
            RuleError::new(&format!("TOML line {line} column {column}"), &message)
        })
}

fn parse_exclusion_entry(value: Value) -> Result<ExclusionEntry, RuleError> {
    let mut fields = Fields::new(value, "exclusion entry", &["base", "path"])?;
    let base = fields.string("base")?;
    Ok(ExclusionEntry {
        base: match base.as_str() {
            "installation-prefix" => ExclusionBase::InstallationPrefix,
            "lib-dirs" => ExclusionBase::LibraryDirs,
            _ => return Err(unknown_variant("exclusion base", &base)),
        },
        path: RelativePath::new(&fields.string("path")?)?,
    })
}

fn parse_shared_root(value: Value) -> Result<RelativePath, RuleError> {
    RelativePath::new(&string(value, "shared-roots")?)
}

struct Fields {
    context: &'static str,
    values: toml::map::Map<String, Value>,
}

impl Fields {
    fn new(value: Value, context: &'static str, allowed: &[&str]) -> Result<Self, RuleError> {
        let Value::Table(values) = value else {
            return Err(RuleError::new(context, "expected a table"));
        };
        if let Some(name) = values.keys().find(|name| !allowed.contains(&name.as_str())) {
            return Err(RuleError::new(
                context,
                &format!("unknown field `{}`", excerpt(name)),
            ));
        }
        Ok(Self { context, values })
    }

    fn required(&mut self, name: &str) -> Result<Value, RuleError> {
        self.values
            .remove(name)
            .ok_or_else(|| RuleError::new(self.context, &format!("missing field `{name}`")))
    }

    fn string(&mut self, name: &str) -> Result<String, RuleError> {
        string(self.required(name)?, name)
    }

    fn list<T>(
        &mut self,
        name: &str,
        parse: fn(Value) -> Result<T, RuleError>,
    ) -> Result<Vec<T>, RuleError> {
        let Some(value) = self.values.remove(name) else {
            return Ok(Vec::new());
        };
        let Value::Array(values) = value else {
            return Err(RuleError::new(name, "expected an array"));
        };
        if values.len() > MAX_RECORDS {
            return Err(RuleError::new(name, "exceeds MAX_RECORDS (4096)"));
        }
        values.into_iter().map(parse).collect()
    }

    fn section<T: Default>(
        &mut self,
        name: &str,
        parse: fn(Value) -> Result<T, RuleError>,
    ) -> Result<T, RuleError> {
        self.values
            .remove(name)
            .map_or_else(|| Ok(T::default()), parse)
    }
}

fn excerpt(value: &str) -> String {
    value
        .chars()
        .take(MAX_ID_BYTES)
        .flat_map(char::escape_default)
        .collect()
}

fn string(value: Value, field: &str) -> Result<String, RuleError> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err(RuleError::new(field, "expected a string")),
    }
}

fn unknown_variant(field: &str, value: &str) -> RuleError {
    RuleError::new(field, &format!("unknown variant `{}`", excerpt(value)))
}

fn parse_trigger(value: Value) -> Result<Trigger, RuleError> {
    let mut fields = Fields::new(value, "trigger", &["soname-prefix"])?;
    Ok(Trigger {
        soname_prefix: fields.string("soname-prefix")?,
    })
}

fn parse_resources(value: Value) -> Result<Resources, RuleError> {
    let mut fields = Fields::new(value, "resources", &["plugins", "data", "helpers"])?;
    Ok(Resources {
        plugins: fields.list("plugins", parse_plugin_group)?,
        data: fields.list("data", parse_data_group)?,
        helpers: fields.list("helpers", parse_helper)?,
    })
}

fn parse_plugin_group(value: Value) -> Result<ResourceGroup, RuleError> {
    parse_resource_group(value, true)
}

fn parse_data_group(value: Value) -> Result<ResourceGroup, RuleError> {
    parse_resource_group(value, false)
}

/// A resource group. Only a plugin group accepts `members`, and a present
/// list must name at least one member, since an absent list already means
/// every compatible member.
fn parse_resource_group(value: Value, plugin: bool) -> Result<ResourceGroup, RuleError> {
    const DATA: &[&str] = &["id", "kind", "candidates", "destination", "requirement"];
    const PLUGIN: &[&str] = &[
        "id",
        "kind",
        "candidates",
        "destination",
        "requirement",
        "members",
    ];
    let mut fields = Fields::new(value, "resource group", if plugin { PLUGIN } else { DATA })?;
    let listed = fields.values.contains_key("members");
    let group = ResourceGroup {
        id: RuleId::new(&fields.string("id")?)?,
        kind: parse_kind(&fields.string("kind")?)?,
        candidates: fields.list("candidates", parse_source_candidate)?,
        destination: RelativePath::new(&fields.string("destination")?)?,
        requirement: parse_requirement(&fields.string("requirement")?)?,
        members: fields.list("members", parse_member)?,
    };
    if listed && group.members.is_empty() {
        return Err(RuleError::new(
            "members",
            &format!(
                "must name at least one member when present: {}",
                group.id.as_str()
            ),
        ));
    }
    Ok(group)
}

fn parse_member(value: Value) -> Result<RelativePath, RuleError> {
    RelativePath::new(&string(value, "members")?)
}

fn parse_helper(value: Value) -> Result<HelperGroup, RuleError> {
    let mut fields = Fields::new(
        value,
        "helper group",
        &[
            "id",
            "group",
            "kind",
            "candidates",
            "destination",
            "requirement",
        ],
    )?;
    Ok(HelperGroup {
        id: RuleId::new(&fields.string("id")?)?,
        group: RuleId::new(&fields.string("group")?)?,
        kind: parse_kind(&fields.string("kind")?)?,
        candidates: fields.list("candidates", parse_source_candidate)?,
        destination: RelativePath::new(&fields.string("destination")?)?,
        requirement: parse_requirement(&fields.string("requirement")?)?,
    })
}

fn parse_kind(value: &str) -> Result<ResourceKind, RuleError> {
    match value {
        "file" => Ok(ResourceKind::File),
        "directory" => Ok(ResourceKind::Directory),
        _ => Err(unknown_variant("kind", value)),
    }
}

fn parse_requirement(value: &str) -> Result<Requirement, RuleError> {
    match value {
        "required" => Ok(Requirement::Required),
        "optional" => Ok(Requirement::Optional),
        _ => Err(unknown_variant("requirement", value)),
    }
}

fn parse_source_candidate(value: Value) -> Result<SourceCandidate, RuleError> {
    let mut fields = Fields::new(value, "source candidate", &["base", "path"])?;
    let base = fields.string("base")?;
    Ok(SourceCandidate {
        base: match base.as_str() {
            "installation-prefix" => SourceBase::InstallationPrefix,
            "library-dir" => SourceBase::LibraryDir,
            _ => return Err(unknown_variant("source base", &base)),
        },
        path: RelativePath::new(&fields.string("path")?)?,
    })
}

fn parse_build(value: Value) -> Result<BuildRules, RuleError> {
    let mut fields = Fields::new(value, "build", &["hooks"])?;
    Ok(BuildRules {
        hooks: fields.list("hooks", parse_build_hook)?,
    })
}

fn parse_build_hook(value: Value) -> Result<BuildHookId, RuleError> {
    let value = string(value, "build hook")?;
    match value.as_str() {
        "alsa_conf_patch" => Ok(BuildHookId::AlsaConfPatch),
        _ => Err(unknown_variant("build hook", &value)),
    }
}

fn parse_runtime(value: Value) -> Result<RuntimeRules, RuleError> {
    let mut fields = Fields::new(value, "runtime", &["paths", "hooks"])?;
    Ok(RuntimeRules {
        paths: fields.list("paths", parse_runtime_path)?,
        hooks: fields.list("hooks", parse_runtime_hook)?,
    })
}

fn parse_runtime_path(value: Value) -> Result<RuntimePathGroup, RuleError> {
    let mut fields = Fields::new(
        value,
        "runtime path",
        &[
            "id",
            "kind",
            "candidates",
            "variables",
            "selection",
            "inheritance",
        ],
    )?;
    let selection = fields.string("selection")?;
    let inheritance = fields.string("inheritance")?;
    Ok(RuntimePathGroup {
        id: RuleId::new(&fields.string("id")?)?,
        kind: parse_kind(&fields.string("kind")?)?,
        candidates: fields.list("candidates", parse_runtime_candidate)?,
        variables: fields.list("variables", parse_variable)?,
        selection: match selection.as_str() {
            "first" => PathSelection::First,
            "all" => PathSelection::All,
            "first-per-library-root" => PathSelection::FirstPerLibraryRoot,
            _ => return Err(unknown_variant("selection", &selection)),
        },
        inheritance: match inheritance.as_str() {
            "preserve" => Inheritance::Preserve,
            "replace" => Inheritance::Replace,
            _ => return Err(unknown_variant("inheritance", &inheritance)),
        },
    })
}

fn parse_variable(value: Value) -> Result<EnvironmentName, RuleError> {
    EnvironmentName::new(&string(value, "environment")?)
}

fn parse_runtime_candidate(value: Value) -> Result<RuntimeCandidate, RuleError> {
    let mut fields = Fields::new(value, "runtime candidate", &["base", "path"])?;
    let base = fields.string("base")?;
    Ok(RuntimeCandidate {
        base: match base.as_str() {
            "package-root" => RuntimeBase::PackageRoot,
            "library-root" => RuntimeBase::LibraryRoot,
            _ => return Err(unknown_variant("runtime base", &base)),
        },
        path: RelativePath::new(&fields.string("path")?)?,
    })
}

fn parse_runtime_hook(value: Value) -> Result<RuntimeHookId, RuleError> {
    let value = string(value, "runtime hook")?;
    match value.as_str() {
        "alsa_config_path_natural_sort" => Ok(RuntimeHookId::AlsaConfigPathNaturalSort),
        "gdk_cache_absolutise" => Ok(RuntimeHookId::GdkCacheAbsolutise),
        "fontconfig_generate" => Ok(RuntimeHookId::FontconfigGenerate),
        "gio_module_paths" => Ok(RuntimeHookId::GioModulePaths),
        "tcl_tk_libraries" => Ok(RuntimeHookId::TclTkLibraries),
        _ => Err(unknown_variant("runtime hook", &value)),
    }
}
