//! ONELF format definitions

#[macro_use]
mod macros;

pub mod cache_layout;
pub mod drivers;
pub mod entry;
pub mod footer;
pub mod manifest;

pub use entry::{Block, Entry, EntryKind, EntryPoint, EntryPointFlags, WorkingDir};
pub use footer::{END_MAGIC, FOOTER_SIZE, Flags, Footer, MAGIC};
pub use manifest::{
    Manifest, ManifestHeader, StringTableBuilder, is_safe_component, symlink_target_within_root,
};
