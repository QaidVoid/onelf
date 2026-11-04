//! Validate canonical framework sources and emit parser-free typed defaults.

// The build script compiles the library's rule model from source and
// uses only part of it.
#[allow(dead_code)]
#[path = "src/rules.rs"]
mod rules;
#[path = "src/rules_toml.rs"]
mod rules_toml;

use crate::rules::*;
use std::error::Error;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=rules");
    println!("cargo:rerun-if-changed=src/rules.rs");
    println!("cargo:rerun-if-changed=src/rules_toml.rs");
    let sources = catalog_sources(Path::new("rules"))?;
    for source in &sources {
        println!("cargo:rerun-if-changed={}", source.display());
    }
    let generated = generate_catalog(sources)?;
    let output = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    fs::write(output.join("framework_catalog.rs"), generated)?;
    Ok(())
}

pub(crate) fn catalog_sources(directory: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut sources = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|ext| ext == "toml")
        {
            sources.push(entry.path());
        }
    }
    sources.sort();
    if sources.is_empty() {
        return Err("the framework catalog must contain TOML sources".into());
    }
    Ok(sources)
}

pub(crate) fn generate_catalog(
    sources: impl IntoIterator<Item = PathBuf>,
) -> Result<String, Box<dyn Error>> {
    let mut sources: Vec<_> = sources.into_iter().collect();
    sources.sort();
    let mut catalog = Vec::with_capacity(sources.len());
    for source in sources {
        let text = fs::read_to_string(&source)?;
        let descriptor = rules_toml::parse_descriptor(&text)
            .map_err(|error| format!("{}: {error}", source.display()))?;
        catalog.push(descriptor);
    }
    catalog.sort_by(|a, b| a.id.cmp(&b.id));
    validate_catalog(&catalog, &[], &[])?;
    let mut output = String::from("fn generated_catalog() -> Vec<Descriptor> { vec![\n");
    for descriptor in &catalog {
        writeln!(output, "{},", descriptor_code(descriptor))?;
    }
    output.push_str("] }\n");
    Ok(output)
}

fn descriptor_code(descriptor: &Descriptor) -> String {
    format!(
        "Descriptor {{ schema: {}, id: {}, triggers: {}, resources: Resources {{ plugins: {}, data: {}, helpers: {} }}, build: BuildRules {{ hooks: {} }}, runtime: RuntimeRules {{ paths: {}, hooks: {} }} }}",
        descriptor.schema,
        id_code(&descriptor.id),
        vector_code(&descriptor.triggers, |trigger| format!(
            "Trigger {{ soname_prefix: {:?}.to_owned() }}",
            trigger.soname_prefix
        )),
        vector_code(&descriptor.resources.plugins, resource_code),
        vector_code(&descriptor.resources.data, resource_code),
        vector_code(&descriptor.resources.helpers, helper_code),
        vector_code(&descriptor.build.hooks, |hook| format!(
            "BuildHookId::{hook:?}"
        )),
        vector_code(&descriptor.runtime.paths, runtime_code),
        vector_code(&descriptor.runtime.hooks, |hook| format!(
            "RuntimeHookId::{hook:?}"
        )),
    )
}

fn vector_code<T>(items: &[T], code: impl Fn(&T) -> String) -> String {
    format!(
        "vec![{}]",
        items.iter().map(code).collect::<Vec<_>>().join(",")
    )
}

fn id_code(id: &RuleId) -> String {
    format!(
        "RuleId::new({:?}).expect(\"validated built-in identity\")",
        id.as_str()
    )
}

fn path_code(path: &RelativePath) -> String {
    format!(
        "RelativePath::new({:?}).expect(\"validated built-in path\")",
        path.as_str()
    )
}

fn source_code(candidate: &SourceCandidate) -> String {
    format!(
        "SourceCandidate {{ base: SourceBase::{:?}, path: {} }}",
        candidate.base,
        path_code(&candidate.path)
    )
}

fn resource_code(resource: &ResourceGroup) -> String {
    format!(
        "ResourceGroup {{ id: {}, kind: ResourceKind::{:?}, candidates: {}, destination: {}, requirement: Requirement::{:?}, members: {} }}",
        id_code(&resource.id),
        resource.kind,
        vector_code(&resource.candidates, source_code),
        path_code(&resource.destination),
        resource.requirement,
        vector_code(&resource.members, path_code)
    )
}

fn helper_code(helper: &HelperGroup) -> String {
    format!(
        "HelperGroup {{ id: {}, group: {}, kind: ResourceKind::{:?}, candidates: {}, destination: {}, requirement: Requirement::{:?} }}",
        id_code(&helper.id),
        id_code(&helper.group),
        helper.kind,
        vector_code(&helper.candidates, source_code),
        path_code(&helper.destination),
        helper.requirement
    )
}

fn runtime_code(path: &RuntimePathGroup) -> String {
    format!(
        "RuntimePathGroup {{ id: {}, kind: ResourceKind::{:?}, candidates: {}, variables: {}, selection: PathSelection::{:?}, inheritance: Inheritance::{:?} }}",
        id_code(&path.id),
        path.kind,
        vector_code(&path.candidates, |candidate| format!(
            "RuntimeCandidate {{ base: RuntimeBase::{:?}, path: {} }}",
            candidate.base,
            path_code(&candidate.path)
        )),
        vector_code(&path.variables, |name| format!(
            "EnvironmentName::new({:?}).expect(\"validated built-in environment name\")",
            name.as_str()
        )),
        path.selection,
        path.inheritance
    )
}
