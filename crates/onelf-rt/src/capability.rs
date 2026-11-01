//! The runtime's ELF capability note.
//!
//! Both built variants embed one note advertising the payload metadata
//! schema version and the features this build supports, placed in a
//! real allocatable note section so the linker carries it in a PT_NOTE
//! segment. Packaging reads the note from the runtime bytes before
//! emitting metadata, so a substitution that would silently ignore
//! required metadata is refused there. The slim build advertises no
//! store import, because fetching pinned artifacts needs the
//! update-only network stack.

use std::arch::global_asm;

use onelf_format::elf::{
    CAPABILITY_NOTE_BYTES, RUNTIME_METADATA_VERSION, RuntimeCapabilities, encode_capability_note,
};

#[cfg(feature = "update")]
const ADVERTISED: RuntimeCapabilities =
    RuntimeCapabilities::SELECTION_VIEW.union(RuntimeCapabilities::STORE_IMPORT);
#[cfg(not(feature = "update"))]
const ADVERTISED: RuntimeCapabilities = RuntimeCapabilities::SELECTION_VIEW;

const NOTE: [u8; CAPABILITY_NOTE_BYTES] =
    encode_capability_note(RUNTIME_METADATA_VERSION, ADVERTISED);

const fn note_word(offset: usize) -> u32 {
    u32::from_le_bytes([
        NOTE[offset],
        NOTE[offset + 1],
        NOTE[offset + 2],
        NOTE[offset + 3],
    ])
}

global_asm!(
    ".pushsection .note.onelf,\"a\",%note",
    ".p2align 2",
    ".long {w0}",
    ".long {w1}",
    ".long {w2}",
    ".asciz \"ONELF\"",
    ".zero 2",
    ".long {w3}",
    ".long {w4}",
    ".long {w5}",
    ".popsection",
    w0 = const note_word(0),
    w1 = const note_word(4),
    w2 = const note_word(8),
    w3 = const note_word(20),
    w4 = const note_word(24),
    w5 = const note_word(28),
);
