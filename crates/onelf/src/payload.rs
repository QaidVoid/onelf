/// Pre-compiled bootstrap payloads for the relative-interpreter technique.

pub const BOOTSTRAP_X86_64: &[u8] = include_bytes!("payload/bootstrap_x86_64.bin");
pub const BOOTSTRAP_AARCH64: &[u8] = include_bytes!("payload/bootstrap_aarch64.bin");

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
