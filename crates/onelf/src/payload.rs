//! Freestanding relative-interpreter payloads embedded into packed binaries.
//!
//! Both the flat bootstrap and the onelf-env constructor cdylib are compiled
//! per target from the `onelf-payloads` crate by this crate's `build.rs`, which
//! points `ONELF_BOOTSTRAP_<ARCH>` / `ONELF_ENV_<ARCH>` at the generated
//! artifacts in `OUT_DIR`.

/// Flat bootstrap binary for the relative-interpreter technique, one per arch.
pub const BOOTSTRAP_X86_64: &[u8] = include_bytes!(env!("ONELF_BOOTSTRAP_X86_64"));
pub const BOOTSTRAP_AARCH64: &[u8] = include_bytes!(env!("ONELF_BOOTSTRAP_AARCH64"));

/// Freestanding onelf-env constructor shared objects, one per arch.
///
/// Bundled into a package's lib/ and injected as a DT_NEEDED of the
/// entrypoint so `.onelf/env` / `.onelf/preload` are re-applied on every
/// exec (survives a sandboxed `clearenv()` + re-exec).
///
/// [`onelf_env_blob`] validates the object is an ELF of the requested machine,
/// so a stale or wrong-arch blob is never injected.
pub const ONELF_ENV_X86_64: &[u8] = include_bytes!(env!("ONELF_ENV_X86_64"));
pub const ONELF_ENV_AARCH64: &[u8] = include_bytes!(env!("ONELF_ENV_AARCH64"));

/// Soname / bundled filename of the onelf-env constructor library.
pub const ONELF_ENV_SONAME: &str = "libonelf-env.so";

/// Return the flat bootstrap binary for `e_machine` (ELF `EM_*`), or `None` if
/// that arch's payload was not built into this onelf (an empty placeholder,
/// e.g. the non-native arch of a single-arch build). The relative-interpreter
/// injection is skipped for such targets.
pub fn bootstrap_blob(e_machine: u16) -> Option<&'static [u8]> {
    const EM_X86_64: u16 = 62;
    const EM_AARCH64: u16 = 183;
    let blob = match e_machine {
        EM_X86_64 => BOOTSTRAP_X86_64,
        EM_AARCH64 => BOOTSTRAP_AARCH64,
        _ => return None,
    };
    (!blob.is_empty()).then_some(blob)
}

/// Return the onelf-env blob for `e_machine` (ELF `EM_*`), or `None` if
/// the architecture is unsupported or its blob wasn't built. The blob is
/// validated as an ELF object of the requested machine so an empty or
/// stale placeholder never gets injected.
pub fn onelf_env_blob(e_machine: u16) -> Option<&'static [u8]> {
    const EM_X86_64: u16 = 62;
    const EM_AARCH64: u16 = 183;
    let blob = match e_machine {
        EM_X86_64 => ONELF_ENV_X86_64,
        EM_AARCH64 => ONELF_ENV_AARCH64,
        _ => return None,
    };
    // ELF magic + 64-bit + machine field (e_machine at offset 18).
    if blob.len() < 20 || &blob[0..4] != b"\x7fELF" {
        return None;
    }
    let m = u16::from_le_bytes([blob[18], blob[19]]);
    if m != e_machine {
        return None;
    }
    Some(blob)
}

// x86_64: `lea XX(%rip), %rsi` at offset 0x0a, displacement at 0x0d, RIP at 0x11.
pub const X86_64_METADATA_LEA_DISP_OFFSET: usize = 0x0d;
pub const X86_64_METADATA_LEA_RIP: usize = 0x11;

// aarch64: `adr x1, _onelf_metadata` at offset 0x10.
// Encodes a 21-bit signed PC-relative offset in the instruction word.
pub const AARCH64_METADATA_ADR_OFFSET: usize = 0x10;

/// Patch the aarch64 `adr` instruction's immediate to point at
/// `target_offset` relative to the instruction at `AARCH64_METADATA_ADR_OFFSET`.
pub fn patch_aarch64_adr(blob: &mut [u8], target_offset: usize) {
    let pc = AARCH64_METADATA_ADR_OFFSET;
    let offset = (target_offset as i64) - (pc as i64);
    assert!(
        (-1048576..=1048575).contains(&offset),
        "adr offset out of range"
    );
    let off = offset as u32;
    let immlo = off & 0x3;
    let immhi = (off >> 2) & 0x7ffff;
    // Read existing instruction, preserve rd and opcode, patch imm fields.
    let mut insn = u32::from_le_bytes(blob[pc..pc + 4].try_into().unwrap());
    insn = (insn & 0x9f00001f) | (immlo << 29) | (immhi << 5);
    blob[pc..pc + 4].copy_from_slice(&insn.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const EM_X86_64: u16 = 62;
    const EM_AARCH64: u16 = 183;

    #[test]
    fn built_env_blobs_are_valid_elf_for_their_machine() {
        // Single-arch builds only embed the native arch (the other is an empty
        // placeholder -> None); validate whichever arches were built.
        for em in [EM_X86_64, EM_AARCH64] {
            if let Some(blob) = onelf_env_blob(em) {
                assert_eq!(&blob[0..4], b"\x7fELF");
                assert_eq!(u16::from_le_bytes([blob[18], blob[19]]), em);
            }
        }
    }

    #[test]
    fn unknown_arch_returns_none() {
        // An unsupported machine has no blob and is never injected.
        assert!(onelf_env_blob(0xffff).is_none());
    }

    #[test]
    fn machine_mismatch_is_rejected() {
        // Asking for aarch64 must never hand back the x86_64 blob.
        if let Some(b) = onelf_env_blob(EM_AARCH64) {
            assert_eq!(u16::from_le_bytes([b[18], b[19]]), EM_AARCH64);
        }
    }

    // The packer patches a metadata-pointer instruction at fixed offsets in the
    // bootstrap trampoline (see inject_relative_interp). If the generated blob's
    // layout drifts, injection silently corrupts the binary, so pin it here.
    #[test]
    fn x86_64_bootstrap_lea_at_expected_offset() {
        // Only when this build embeds the x86_64 bootstrap.
        let Some(b) = bootstrap_blob(EM_X86_64) else {
            return;
        };
        // `_onelf_start` preamble then `lea rsi, [rip+disp]` = `48 8d 35` at
        // 0x0a, disp32 at 0x0d, next instruction (RIP) at 0x11.
        assert_eq!(
            &b[0x00..0x0a],
            &[0x48, 0x89, 0xe5, 0x48, 0x83, 0xe4, 0xf0, 0x48, 0x89, 0xef]
        );
        assert_eq!(&b[0x0a..0x0d], &[0x48, 0x8d, 0x35], "lea opcode moved");
        assert_eq!(X86_64_METADATA_LEA_DISP_OFFSET, 0x0d);
        assert_eq!(X86_64_METADATA_LEA_RIP, 0x11);
    }

    #[test]
    fn aarch64_bootstrap_adr_at_expected_offset() {
        let Some(b) = bootstrap_blob(EM_AARCH64) else {
            return;
        };
        // `adr x1, _onelf_metadata` at 0x10; masking off the immediate leaves
        // the ADR opcode with destination register x1.
        assert_eq!(AARCH64_METADATA_ADR_OFFSET, 0x10);
        let insn = u32::from_le_bytes(b[0x10..0x14].try_into().unwrap());
        assert_eq!(insn & 0x9f00_001f, 0x1000_0001, "adr x1 opcode moved");
    }
}
