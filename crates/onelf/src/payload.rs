/// Pre-compiled bootstrap payloads for the relative-interpreter technique.

pub const BOOTSTRAP_X86_64: &[u8] = include_bytes!("payload/bootstrap_x86_64.bin");
pub const BOOTSTRAP_AARCH64: &[u8] = include_bytes!("payload/bootstrap_aarch64.bin");

/// Freestanding onelf-env constructor shared objects, one per arch.
///
/// Bundled into a package's lib/ and injected as a DT_NEEDED of the
/// entrypoint so `.onelf/env` / `.onelf/preload` are re-applied on every
/// exec (survives a sandboxed `clearenv()` + re-exec). Built from
/// `payload/onelf_env.c` by `payload/Makefile`.
///
/// These are prebuilt blobs checked into the repo (like the bootstrap
/// blobs). A blob that hasn't been built for an arch is an empty
/// placeholder; [`onelf_env_blob`] returns `None` for it and the
/// bundler falls back to runtime-only env for that target.
pub const ONELF_ENV_X86_64: &[u8] = include_bytes!("payload/onelf_env_x86_64.so");
pub const ONELF_ENV_AARCH64: &[u8] = include_bytes!("payload/onelf_env_aarch64.so");

/// Soname / bundled filename of the onelf-env constructor library.
pub const ONELF_ENV_SONAME: &str = "libonelf-env.so";

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
