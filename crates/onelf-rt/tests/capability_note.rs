//! The built runtime binary carries its capability note.

use onelf_format::elf::{
    RUNTIME_METADATA_VERSION, RuntimeCapabilities, RuntimeNote, probe_capability_note,
};

#[test]
fn built_runtime_advertises_its_supported_features() {
    let bytes = std::fs::read(env!("CARGO_BIN_EXE_onelf-rt")).expect("runtime binary readable");
    let note = probe_capability_note(&bytes).expect("runtime binary probes");
    let RuntimeNote::Present {
        metadata_version,
        capabilities,
    } = note.clone()
    else {
        panic!("runtime binary carries no probeable capability note: {note:?}");
    };
    assert_eq!(metadata_version, RUNTIME_METADATA_VERSION);
    assert!(capabilities.contains(RuntimeCapabilities::SELECTION_VIEW));
    if cfg!(feature = "update") {
        assert!(capabilities.contains(RuntimeCapabilities::STORE_IMPORT));
    } else {
        assert!(!capabilities.contains(RuntimeCapabilities::STORE_IMPORT));
    }
}
