//! Capability note encoding, probing and compatibility gating.

use onelf_format::elf::{
    CAPABILITY_NOTE_BYTES, NOTE_NAME, NOTE_TYPE, RUNTIME_METADATA_VERSION, RuntimeCapabilities,
    RuntimeNote, RuntimeRequirements, encode_capability_note, probe_capability_note,
};

const PT_NOTE: u32 = 4;

fn all_caps() -> RuntimeCapabilities {
    RuntimeCapabilities::SELECTION_VIEW.union(RuntimeCapabilities::STORE_IMPORT)
}

/// One note entry laid out for `align`-aligned segments.
fn note_entry(name: &[u8], ntype: u32, desc: &[u8], align: usize) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&(name.len() as u32).to_le_bytes());
    v.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    v.extend_from_slice(&ntype.to_le_bytes());
    v.extend_from_slice(name);
    while v.len() % align != 0 {
        v.push(0);
    }
    v.extend_from_slice(desc);
    while v.len() % align != 0 {
        v.push(0);
    }
    v
}

fn capability_entry(align: usize) -> Vec<u8> {
    note_entry(
        b"ONELF\0",
        NOTE_TYPE,
        &encode_capability_note(RUNTIME_METADATA_VERSION, all_caps())[20..],
        align,
    )
}

fn gnu_entry() -> Vec<u8> {
    note_entry(b"GNU\0", 1, &[1, 2, 3, 4], 4)
}

/// A little-endian ELF64 whose PT_NOTE segments carry the given blobs.
fn elf64_with_segments(segments: &[Vec<u8>], align: u64) -> Vec<u8> {
    let phoff = 64usize;
    let phentsize = 56usize;
    let mut v = vec![0u8; phoff + phentsize * segments.len()];
    v[0..4].copy_from_slice(b"\x7fELF");
    v[4] = 2;
    v[5] = 1;
    v[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
    v[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes());
    v[56..58].copy_from_slice(&(segments.len() as u16).to_le_bytes());
    let mut body = phoff + phentsize * segments.len();
    for (i, seg) in segments.iter().enumerate() {
        let off = phoff + phentsize * i;
        v[off..off + 4].copy_from_slice(&PT_NOTE.to_le_bytes());
        v[off + 8..off + 16].copy_from_slice(&(body as u64).to_le_bytes());
        v[off + 32..off + 40].copy_from_slice(&(seg.len() as u64).to_le_bytes());
        v[off + 48..off + 56].copy_from_slice(&align.to_le_bytes());
        v.extend_from_slice(seg);
        body += seg.len();
    }
    v
}

/// A little-endian ELF32 whose PT_NOTE segments carry the given blobs.
fn elf32_with_segments(segments: &[Vec<u8>], align: u32) -> Vec<u8> {
    let phoff = 52usize;
    let phentsize = 32usize;
    let mut v = vec![0u8; phoff + phentsize * segments.len()];
    v[0..4].copy_from_slice(b"\x7fELF");
    v[4] = 1;
    v[5] = 1;
    v[28..32].copy_from_slice(&(phoff as u32).to_le_bytes());
    v[42..44].copy_from_slice(&(phentsize as u16).to_le_bytes());
    v[44..46].copy_from_slice(&(segments.len() as u16).to_le_bytes());
    let mut body = phoff + phentsize * segments.len();
    for (i, seg) in segments.iter().enumerate() {
        let off = phoff + phentsize * i;
        v[off..off + 4].copy_from_slice(&PT_NOTE.to_le_bytes());
        v[off + 4..off + 8].copy_from_slice(&(body as u32).to_le_bytes());
        v[off + 16..off + 20].copy_from_slice(&(seg.len() as u32).to_le_bytes());
        v[off + 24..off + 28].copy_from_slice(&align.to_le_bytes());
        v.extend_from_slice(seg);
        body += seg.len();
    }
    v
}

fn elf64_without_notes() -> Vec<u8> {
    elf64_with_segments(&[], 4)
}

#[test]
fn encoded_note_has_the_canonical_layout() {
    let note = encode_capability_note(1, all_caps());
    let expected: [u8; CAPABILITY_NOTE_BYTES] = [
        6, 0, 0, 0, // namesz
        12, 0, 0, 0, // descsz
        1, 0, 0, 0, // ntype
        b'O', b'N', b'E', b'L', b'F', 0, 0, 0, // name, padded
        b'O', b'C', b'A', b'P', // desc magic
        1, 0, // metadata version
        0, 0, // reserved
        3, 0, 0, 0, // capability bits
    ];
    assert_eq!(note, expected);
}

#[test]
fn encoded_note_round_trips_through_probe() {
    for caps in [
        RuntimeCapabilities::empty(),
        RuntimeCapabilities::SELECTION_VIEW,
        all_caps(),
    ] {
        let note = encode_capability_note(RUNTIME_METADATA_VERSION, caps);
        let elf = elf64_with_segments(&[note.to_vec()], 4);
        assert_eq!(
            probe_capability_note(&elf).unwrap(),
            RuntimeNote::Present {
                metadata_version: RUNTIME_METADATA_VERSION,
                capabilities: caps,
            }
        );
    }
}

#[test]
fn elf32_and_align8_segments_probe_the_same() {
    for elf in [
        elf32_with_segments(&[capability_entry(4)], 4),
        elf64_with_segments(&[capability_entry(8)], 8),
        elf32_with_segments(&[capability_entry(8)], 8),
    ] {
        assert_eq!(
            probe_capability_note(&elf).unwrap(),
            RuntimeNote::Present {
                metadata_version: RUNTIME_METADATA_VERSION,
                capabilities: all_caps(),
            }
        );
    }
}

#[test]
fn note_among_foreign_notes_is_found() {
    let elf = elf64_with_segments(&[gnu_entry(), capability_entry(4)], 4);
    assert!(matches!(
        probe_capability_note(&elf).unwrap(),
        RuntimeNote::Present { .. }
    ));
    let elf = elf64_with_segments(&[capability_entry(4), gnu_entry()], 4);
    assert!(matches!(
        probe_capability_note(&elf).unwrap(),
        RuntimeNote::Present { .. }
    ));
    let elf = elf64_with_segments(&[vec![], capability_entry(4), gnu_entry()], 4);
    assert!(matches!(
        probe_capability_note(&elf).unwrap(),
        RuntimeNote::Present { .. }
    ));
}

#[test]
fn absent_note_is_distinct_from_malformed() {
    assert_eq!(
        probe_capability_note(&elf64_without_notes()).unwrap(),
        RuntimeNote::Absent
    );
    // A foreign note, and one naming the vendor under another type, are
    // absence as far as this probe is concerned.
    assert_eq!(
        probe_capability_note(&elf64_with_segments(&[gnu_entry()], 4)).unwrap(),
        RuntimeNote::Absent
    );
    assert_eq!(
        probe_capability_note(&elf64_with_segments(
            &[note_entry(b"ONELF\0", 7, &[9; 12], 4)],
            4
        ))
        .unwrap(),
        RuntimeNote::Absent
    );
    // A note claiming the vendor and type but with a broken description is
    // never absence.
    assert!(matches!(
        probe_capability_note(&elf64_with_segments(
            &[note_entry(b"ONELF\0", NOTE_TYPE, &[9; 12], 4)],
            4
        ))
        .unwrap(),
        RuntimeNote::Incompatible(_)
    ));
}

#[test]
fn truncated_notes_never_probe_present() {
    let note = encode_capability_note(RUNTIME_METADATA_VERSION, all_caps());
    for len in 0..note.len() {
        let elf = elf64_with_segments(&[note[..len].to_vec()], 4);
        let probed = probe_capability_note(&elf).unwrap();
        assert!(
            !matches!(probed, RuntimeNote::Present { .. }),
            "truncation at {len} probed present"
        );
    }
}

#[test]
fn bitflips_never_forge_an_acceptable_advertisement() {
    let note = encode_capability_note(RUNTIME_METADATA_VERSION, all_caps());
    for i in 0..note.len() {
        for bit in 0..8 {
            let mut flipped = note;
            flipped[i] ^= 1 << bit;
            let elf = elf64_with_segments(&[flipped.to_vec()], 4);
            match probe_capability_note(&elf).unwrap() {
                RuntimeNote::Present {
                    metadata_version,
                    capabilities,
                } => {
                    assert!(metadata_version >= 1);
                    assert!(
                        capabilities
                            .difference(RuntimeCapabilities::ALL_KNOWN)
                            .is_empty()
                    );
                }
                RuntimeNote::Absent | RuntimeNote::Incompatible(_) => {}
            }
        }
    }
}

#[test]
fn invalid_descriptions_are_incompatible() {
    let cases: Vec<Vec<u8>> = vec![
        note_entry(b"ONELF\0", NOTE_TYPE, b"XCAP\x01\0\0\0\0\0\0\0", 4),
        note_entry(b"ONELF\0", NOTE_TYPE, b"OCAP\x00\0\0\0\0\0\0\0", 4),
        note_entry(b"ONELF\0", NOTE_TYPE, b"OCAP\x01\0\x01\0\0\0\0\0", 4),
        note_entry(b"ONELF\0", NOTE_TYPE, b"OCAP\x01\0\0\0\x08\0\0\0", 4),
    ];
    for desc in cases {
        assert!(matches!(
            probe_capability_note(&elf64_with_segments(&[desc], 4)).unwrap(),
            RuntimeNote::Incompatible(_)
        ));
    }
    // A note of the right vendor and type but a non-canonical description
    // width is not a readable advertisement: absence, not corruption.
    for desc in [vec![9u8; 11], vec![9u8; 13]] {
        assert!(matches!(
            probe_capability_note(&elf64_with_segments(
                &[note_entry(b"ONELF\0", NOTE_TYPE, &desc, 4)],
                4
            ))
            .unwrap(),
            RuntimeNote::Absent
        ));
    }
}

#[test]
fn oversized_declared_lengths_are_abandoned_without_panic() {
    // A foreign note with a nonsense declared size, and a note of ours
    // whose declared description is not the canonical width, are skipped
    // without following their lengths.
    let mut corrupt = gnu_entry();
    corrupt[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        probe_capability_note(&elf64_with_segments(&[corrupt], 4)).unwrap(),
        RuntimeNote::Absent
    ));

    let mut greedy = note_entry(b"ONELF\0", NOTE_TYPE, &[9; 12], 4);
    greedy[4..8].copy_from_slice(&4096u32.to_le_bytes());
    assert!(matches!(
        probe_capability_note(&elf64_with_segments(&[greedy], 4)).unwrap(),
        RuntimeNote::Absent
    ));
}

#[test]
fn corrupt_segment_does_not_hide_a_later_segment() {
    let mut corrupt = gnu_entry();
    corrupt[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let elf = elf64_with_segments(&[corrupt, capability_entry(4)], 4);
    assert!(matches!(
        probe_capability_note(&elf).unwrap(),
        RuntimeNote::Present { .. }
    ));
}

#[test]
fn segment_bounds_drive_the_scan() {
    // Segment metadata pointing outside the file.
    let mut elf = elf64_with_segments(&[capability_entry(4)], 4);
    elf[72..80].copy_from_slice(&u64::MAX.to_le_bytes()); // p_offset
    assert_eq!(probe_capability_note(&elf).unwrap(), RuntimeNote::Absent);

    let mut elf = elf64_with_segments(&[capability_entry(4)], 4);
    elf[96..104].copy_from_slice(&u64::MAX.to_le_bytes()); // p_filesz
    assert_eq!(probe_capability_note(&elf).unwrap(), RuntimeNote::Absent);

    // A table claiming far more entries than the file holds.
    let mut elf = elf64_with_segments(&[capability_entry(4)], 4);
    elf[56..58].copy_from_slice(&u16::MAX.to_le_bytes());
    assert!(matches!(
        probe_capability_note(&elf).unwrap(),
        RuntimeNote::Present { .. } | RuntimeNote::Absent
    ));
}

#[test]
fn non_elf_bytes_are_a_probe_error() {
    let short_elf = {
        let mut v = vec![0u8; 20];
        v[0..4].copy_from_slice(b"\x7fELF");
        v
    };
    let unknown_class = {
        let mut v = vec![0u8; 64];
        v[0..4].copy_from_slice(b"\x7fELF");
        v[4] = 5;
        v
    };
    for data in [&b""[..], b"hello", &short_elf[..], &unknown_class[..]] {
        assert!(probe_capability_note(data).is_err());
    }
}

#[test]
fn compatibility_gate_accepts_and_rejects_each_case() {
    let present = RuntimeNote::Present {
        metadata_version: RUNTIME_METADATA_VERSION,
        capabilities: RuntimeCapabilities::SELECTION_VIEW,
    };
    let present_all = RuntimeNote::Present {
        metadata_version: RUNTIME_METADATA_VERSION,
        capabilities: all_caps(),
    };
    let present_newer = RuntimeNote::Present {
        metadata_version: RUNTIME_METADATA_VERSION + 1,
        capabilities: all_caps(),
    };
    let RuntimeNote::Incompatible(inner) = probe_capability_note(&elf64_with_segments(
        &[note_entry(b"ONELF\0", NOTE_TYPE, &[9; 12], 4)],
        4,
    ))
    .unwrap() else {
        panic!("broken description must probe incompatible");
    };
    let incompatible = RuntimeNote::Incompatible(inner);

    assert!(probe_capability_note(&elf64_without_notes()).unwrap() == RuntimeNote::Absent);
    let absent = RuntimeNote::Absent;

    for (note, required, ok) in [
        (&absent, RuntimeRequirements::NONE, true),
        (&absent, RuntimeRequirements::metadata(1), false),
        (
            &absent,
            RuntimeRequirements {
                metadata_version: 0,
                capabilities: RuntimeCapabilities::SELECTION_VIEW,
            },
            false,
        ),
        (&incompatible, RuntimeRequirements::NONE, false),
        (&incompatible, RuntimeRequirements::metadata(1), false),
        (&present, RuntimeRequirements::NONE, true),
        (&present, RuntimeRequirements::metadata(1), true),
        (&present, RuntimeRequirements::metadata(2), false),
        (
            &present,
            RuntimeRequirements {
                metadata_version: 1,
                capabilities: all_caps(),
            },
            false,
        ),
        (&present_all, RuntimeRequirements::metadata(1), true),
        (&present_newer, RuntimeRequirements::metadata(1), true),
    ] {
        let result = onelf_format::elf::check_runtime_support(note, required);
        assert_eq!(result.is_ok(), ok, "{note:?} vs {required:?}");
    }
}

#[test]
fn rejection_errors_name_what_is_missing() {
    let absent_err = onelf_format::elf::check_runtime_support(
        &RuntimeNote::Absent,
        RuntimeRequirements::metadata(1),
    )
    .unwrap_err();
    assert!(absent_err.to_string().contains("no capability note"));

    let present = RuntimeNote::Present {
        metadata_version: 1,
        capabilities: RuntimeCapabilities::SELECTION_VIEW,
    };
    let err = onelf_format::elf::check_runtime_support(
        &present,
        RuntimeRequirements {
            metadata_version: 1,
            capabilities: all_caps(),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("store-import"));

    let old = RuntimeNote::Present {
        metadata_version: 1,
        capabilities: all_caps(),
    };
    let err = onelf_format::elf::check_runtime_support(&old, RuntimeRequirements::metadata(2))
        .unwrap_err();
    assert!(err.to_string().contains("version 1") && err.to_string().contains("2"));
}

#[test]
fn vendor_name_matches_the_encoder() {
    assert_eq!(NOTE_NAME, b"ONELF");
}
