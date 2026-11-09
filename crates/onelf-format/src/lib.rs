//! ONELF format definitions

#[macro_use]
mod macros;

pub mod anchors;
pub mod cache_layout;
pub mod catalog;
pub mod desktop;
pub mod drivers;
pub mod elf;
pub mod entry;
pub mod footer;
pub mod knowledge;
pub mod ldso;
pub mod manifest;
pub mod paths;
pub mod payload;
pub mod plugindirs;
pub mod provider_index;
pub mod reader;
pub mod resolve;
pub mod rules;
#[cfg(feature = "rules-toml")]
pub mod rules_toml;
pub mod runtime_metadata;
pub mod update;
pub mod verdef;

pub use entry::{Block, Entry, EntryKind, EntryPoint, EntryPointFlags, WorkingDir};
pub use footer::{END_MAGIC, FOOTER_SIZE, Flags, Footer, HostLibsPolicy, MAGIC};
pub use manifest::{
    Manifest, ManifestHeader, StringTableBuilder, is_safe_component, symlink_target_within_root,
};
