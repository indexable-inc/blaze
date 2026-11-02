//! The stubs' bytes, for both architectures on any host: layout, entries, and the AArch64
//! encoders against instructions as an assembler writes them.

use crate::{
    LazyId,
    stub::{self, Abi, CELL, ENTRY, Isa, RESOLVER, StubError, X86Save, generate},
};

const ISAS: [Isa; 6] = [
    Isa::X86_64 { abi: Abi::SysV, save: X86Save::Fx },
    Isa::X86_64 { abi: Abi::SysV, save: X86Save::Xsave { mask: 0x7, size: 832 } },
    Isa::X86_64 { abi: Abi::Win64, save: X86Save::Xsavec { mask: 0xe7, size: 2696 } },
    Isa::X86_64 { abi: Abi::SysV, save: X86Save::Xsave { mask: 0xff, size: 5000 } },
    Isa::Aarch64 { sve: false },
    Isa::Aarch64 { sve: true },
];

#[test]
fn the_cell_holds_the_hook_and_entries_follow_the_resolver() {
    for isa in ISAS {
        let stubs = generate(&isa, 5, 0x1122_3344_5566_7788).unwrap();
        let layout = stubs.layout;
        assert_eq!(&stubs.bytes[CELL as usize..8], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert!(layout.entries > RESOLVER && layout.entries.is_multiple_of(ENTRY));
        assert_eq!(layout.len as usize, stubs.bytes.len());
        assert_eq!(layout.len, layout.entries + 5 * ENTRY);
        for id in 0..5 {
            assert_eq!(layout.id_at(layout.entry(LazyId(id))), Some(LazyId(id)));
        }
        assert_eq!(layout.id_at(layout.entries + 1), None);
        assert_eq!(layout.id_at(layout.entries + 5 * ENTRY), None);
    }
}

#[test]
fn an_x86_entry_pushes_its_id_and_jumps_to_the_resolver() {
    let stubs = generate(&ISAS[0], 3, 0).unwrap();
    for id in 0..3u32 {
        let at = stubs.layout.entry(LazyId(id)) as usize;
        let entry = &stubs.bytes[at..at + ENTRY as usize];
        assert_eq!(&entry[..4], [0xf3, 0x0f, 0x1e, 0xfa], "endbr64");
        assert_eq!(entry[4], 0x68);
        assert_eq!(u32::from_le_bytes(entry[5..9].try_into().unwrap()), id);
        assert_eq!(entry[9], 0xe9);
        let rel = i32::from_le_bytes(entry[10..14].try_into().unwrap());
        assert_eq!(at as i64 + 14 + i64::from(rel), i64::from(RESOLVER));
    }
}

#[test]
fn the_x86_resolver_reads_the_cell_and_ends_in_a_jump_through_r11() {
    for isa in &ISAS[..4] {
        let stubs = generate(isa, 1, 0).unwrap();
        let code = &stubs.bytes[RESOLVER as usize..stubs.layout.entries as usize];
        let load = code.windows(3).position(|it| it == [0x48, 0x8b, 0x05]).expect("mov rax, [rip + cell]");
        let disp = i32::from_le_bytes(code[load + 3..load + 7].try_into().unwrap());
        assert_eq!(i64::from(RESOLVER) + load as i64 + 7 + i64::from(disp), i64::from(CELL));
        assert!(code.windows(5).any(|it| it == [0x41, 0x5b, 0x41, 0xff, 0xe3]), "pop r11; jmp r11");
        let trap = code.windows(2).rposition(|it| it == [0x0f, 0x0b]).expect("ud2");
        let jz = code.windows(3).position(|it| it == [0x48, 0x85, 0xc0]).expect("test rax, rax") + 3;
        assert_eq!(code[jz], 0x74);
        assert_eq!(jz + 2 + code[jz + 1] as usize, trap);
        let probes = code.windows(8).filter(|it| *it == [0x48, 0xc7, 0x04, 0x24, 0, 0, 0, 0]).count();
        let size = match isa {
            Isa::X86_64 { save, .. } => save.size(),
            Isa::Aarch64 { .. } => unreachable!(),
        };
        assert_eq!(probes as u32, size.div_ceil(2048));
    }
}

#[test]
fn win64_passes_the_id_in_ecx_with_shadow_space() {
    let stubs = generate(&ISAS[2], 1, 0).unwrap();
    let code = &stubs.bytes[RESOLVER as usize..];
    assert!(code.windows(7).any(|it| it == [0x8b, 0x4d, 0x08, 0x48, 0x83, 0xec, 0x20]));
    assert!(code.windows(4).any(|it| it == [0x48, 0x83, 0xc4, 0x20]));
    let sysv = generate(&ISAS[0], 1, 0).unwrap();
    assert!(sysv.bytes.windows(3).any(|it| it == [0x8b, 0x7d, 0x08]));
}

fn words(bytes: &[u8]) -> Vec<u32> {
    bytes.chunks_exact(4).map(|it| u32::from_le_bytes(it.try_into().unwrap())).collect()
}

#[test]
fn an_aarch64_entry_puts_its_id_in_x16_and_branches_to_the_resolver() {
    let stubs = generate(&ISAS[4], 3, 0).unwrap();
    for id in [0u32, 1, 2] {
        let at = stubs.layout.entry(LazyId(id));
        let entry = words(&stubs.bytes[at as usize..(at + ENTRY) as usize]);
        assert_eq!(entry[0], 0xd503_245f, "bti c");
        assert_eq!(entry[1], 0x5280_0010 | id << 5, "movz w16, #id");
        assert_eq!(entry[2], 0x72a0_0010, "movk w16, #0, lsl #16");
        let imm26 = entry[3] & 0x03ff_ffff;
        let offset = ((imm26 << 6) as i32 >> 6) * 4;
        assert_eq!(entry[3] >> 26, 0b000101);
        assert_eq!(i64::from(at) + 12 + i64::from(offset), i64::from(RESOLVER));
    }
}

#[test]
fn aarch64_encoders_match_the_assembler() {
    use crate::stub::aarch64::*;
    // As `llvm-mc -triple=aarch64 -mattr=+sve -show-encoding` writes them.
    let cases: &[(u32, u32, &str)] = &[
        (stp_x(0, 1, 31, 0), 0xa900_07e0, "stp x0, x1, [sp]"),
        (stp_x(14, 15, 31, 112), 0xa907_3fee, "stp x14, x15, [sp, #112]"),
        (ldp_x(2, 3, 31, 16), 0xa941_0fe2, "ldp x2, x3, [sp, #16]"),
        (stp_q(0, 1, 31, 0), 0xad00_07e0, "stp q0, q1, [sp]"),
        (stp_q(30, 31, 31, 480), 0xad0f_7ffe, "stp q30, q31, [sp, #480]"),
        (ldp_q(2, 3, 31, 32), 0xad41_0fe2, "ldp q2, q3, [sp, #32]"),
        (sub_imm(31, 31, 128), 0xd102_03ff, "sub sp, sp, #128"),
        (add_imm(31, 31, 512), 0x9108_03ff, "add sp, sp, #512"),
        (addvl(31, 31, -1), 0x043f_57ff, "addvl sp, sp, #-1"),
        (addvl(31, 31, 32), 0x043f_541f, "addvl sp, sp, #32"),
        (str_z(0, 31, 0), 0xe580_43e0, "str z0, [sp]"),
        (str_z(31, 31, 31), 0xe583_5fff, "str z31, [sp, #31, mul vl]"),
        (ldr_z(5, 31, 9), 0x8581_47e5, "ldr z5, [sp, #9, mul vl]"),
        (str_p(0, 31, 16), 0xe582_03e0, "str p0, [sp, #16, mul vl]"),
        (ldr_p(15, 31, 15), 0x8581_1fef, "ldr p15, [sp, #15, mul vl]"),
        (rdffr(0), 0x2519_f000, "rdffr p0.b"),
        (wrffr(0), 0x2528_9000, "wrffr p0.b"),
        (movz_w(16, 0x1234), 0x5282_4690, "movz w16, #0x1234"),
        (movk_w16(16, 0x7fff), 0x72af_fff0, "movk w16, #0x7fff, lsl #16"),
        (b(-8), 0x17ff_fffe, "b .-8"),
        (ldr_literal(16, -64), 0x58ff_fe10, "ldr x16, .-64"),
        (cbz_x(17, 40), 0xb400_0151, "cbz x17, .+40"),
    ];
    for &(ours, want, what) in cases {
        assert_eq!(ours, want, "{what}: {ours:#010x} where the assembler writes {want:#010x}");
    }
}

#[test]
fn the_aarch64_resolver_calls_the_cell_and_traps_on_zero() {
    for sve in [false, true] {
        let stubs = generate(&Isa::Aarch64 { sve }, 1, 0).unwrap();
        let code = words(&stubs.bytes[RESOLVER as usize..stubs.layout.entries as usize]);
        let ldr = code.iter().position(|it| it & 0xff00_001f == 0x5800_0010).expect("ldr x16, cell");
        let imm19 = (code[ldr] >> 5) & 0x7ffff;
        let offset = ((imm19 << 13) as i32 >> 13) * 4;
        assert_eq!(i64::from(RESOLVER) + 4 * ldr as i64 + i64::from(offset), i64::from(CELL));
        assert_eq!(code[ldr + 1], 0xd63f_0200, "blr x16");
        let cbz = ldr + 3;
        assert_eq!(code[cbz] & 0xff00_001f, 0xb400_0011, "cbz x17");
        let target = cbz as i64 + i64::from((((code[cbz] >> 5) & 0x7ffff) << 13) as i32 >> 13);
        assert_eq!(code[target as usize], 0xd420_0020, "brk #1");
        assert!(code.contains(&0xd61f_0220), "br x17");
        // Every store of the frame is undone: as many SVE loads as stores, and the stack
        // grows only through probed steps.
        let zstores = code.iter().filter(|it| *it & 0xffc0_e000 == 0xe580_4000).count();
        let zloads = code.iter().filter(|it| *it & 0xffc0_e000 == 0x8580_4000).count();
        assert_eq!((zstores, zloads), if sve { (32, 32) } else { (0, 0) });
    }
}

#[test]
fn a_region_too_big_for_its_branches_is_refused() {
    let most = stub::most(&ISAS[4]);
    assert!(matches!(generate(&ISAS[4], most + 1, 0), Err(StubError::TooMany { .. })));
    assert!(most >= 1 << 22);
}