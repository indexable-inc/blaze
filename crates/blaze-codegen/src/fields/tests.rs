//! A record is free of addresses: placed at two bases with two sets of targets, a function's
//! patched images differ, and each cleared is the stored bytes; its relocations round-trip
//! through the stored blob; a field holding bits is refused.
//!
//! [`place`] is a model of the launcher's patcher for the kinds these tests use, kept here
//! only to make placed images: the launcher's `place::patch` is the one that runs.

use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_sim::Config;

use super::*;
use crate::{
    Arch, CompiledFunction, InstanceKey, Relocation, Relocs, Target, blob, template::Template,
};

fn key(n: u8) -> InstanceKey {
    InstanceKey(ContentId([n; 32]))
}

fn words(insns: &[u32]) -> Vec<u8> {
    insns.iter().flat_map(|it| it.to_le_bytes()).collect()
}

/// An AArch64 function calling `callee`, taking a static's address and loading a symbol's
/// GOT cell: `bl; adrp; add; adrp; ldr; ret`, every field zero.
fn aarch64() -> CompiledFunction {
    let callee = Target::Function(key(2));
    let item = Target::Static { item: ItemKey(ContentId([3; 32])), instance: ContentId([4; 32]) };
    let symbol = Target::Extern("memcpy".to_owned());
    let rel = |at, kind, target: &Target, addend| Relocation {
        at,
        kind,
        target: target.clone(),
        addend,
    };
    CompiledFunction {
        key: key(1),
        arch: Arch::Aarch64,
        align: 4,
        code: words(&[0x9400_0000, 0x9000_0000, 0x9100_0000, 0x9000_0001, 0xF940_0021, 0xD65F_03C0]),
        relocs: vec![
            rel(0, RelocKind::Call26, &callee, 0),
            rel(4, RelocKind::AdrpPage21, &item, 16),
            rel(8, RelocKind::PageOffset12, &item, 16),
            rel(12, RelocKind::GotPage21, &symbol, 0),
            rel(16, RelocKind::GotPageOffset12, &symbol, 0),
        ]
        .into(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    }
}

/// An x86-64 function: `call rel32; movabs rax, imm64; ret`.
fn x86_64() -> CompiledFunction {
    let callee = Target::Function(key(5));
    let item = Target::Static { item: ItemKey(ContentId([6; 32])), instance: ContentId([7; 32]) };
    let mut code = vec![0xe8, 0, 0, 0, 0, 0x48, 0xb8];
    code.extend_from_slice(&[0; 8]);
    code.push(0xc3);
    CompiledFunction {
        code,
        arch: Arch::X86_64,
        relocs: vec![
            Relocation { at: 1, kind: RelocKind::PcRel32, target: callee, addend: -4 },
            Relocation { at: 7, kind: RelocKind::Abs64, target: item, addend: 8 },
        ]
        .into(),
        ..aarch64()
    }
}

/// Where a build put the function and each of its targets (a GOT cell for an extern).
struct Placement {
    base: u64,
    targets: [u64; 3],
}

fn or_into(code: &mut [u8], at: u32, bits: u32) {
    let at = at as usize;
    let word = u32::from_le_bytes(code[at..at + 4].try_into().expect("four bytes")) | bits;
    code[at..at + 4].copy_from_slice(&word.to_le_bytes());
}

/// `function` patched at `placed`: the model patcher, for the kinds above.
fn place(function: &CompiledFunction, placed: &Placement) -> Vec<u8> {
    let mut code = function.code.clone();
    let targets = function.relocs.targets();
    for it in function.relocs.iter() {
        let index = targets.iter().position(|t| t == it.target).expect("a target of the table");
        let s = placed.targets[index].wrapping_add_signed(it.addend);
        let p = placed.base + u64::from(it.at);
        let at = it.at as usize;
        match it.kind {
            RelocKind::Abs64 => code[at..at + 8].copy_from_slice(&s.to_le_bytes()),
            RelocKind::PcRel32 => {
                let rel = i32::try_from(s.wrapping_sub(p) as i64).expect("within 2 GiB");
                code[at..at + 4].copy_from_slice(&rel.to_le_bytes());
            }
            RelocKind::Call26 => {
                let off = s.wrapping_sub(p) as i64;
                assert!(off % 4 == 0 && (-(1 << 27)..1 << 27).contains(&off), "within 128 MiB");
                or_into(&mut code, it.at, ((off >> 2) as u32) & 0x03FF_FFFF);
            }
            RelocKind::AdrpPage21 | RelocKind::GotPage21 => {
                let pages = ((s & !0xfff) as i64 - (p & !0xfff) as i64) >> 12;
                let pages = pages as u32;
                or_into(&mut code, it.at, ((pages & 3) << 29) | (((pages >> 2) & 0x7FFFF) << 5));
            }
            RelocKind::PageOffset12 | RelocKind::GotPageOffset12 => {
                let insn = u32::from_le_bytes(code[at..at + 4].try_into().expect("four bytes"));
                let scale = match insn & 0x3B00_0000 == 0x3900_0000 {
                    true => insn >> 30,
                    false => 0,
                };
                or_into(&mut code, it.at, (((s & 0xfff) >> scale) as u32) << 10);
            }
            other => panic!("the model patcher has no {other:?}"),
        }
    }
    code
}

fn placements() -> [Placement; 2] {
    [
        Placement { base: 0x1_0000_0000, targets: [0x1_0040_0000, 0x1_2000_0000, 0x1_3000_0008] },
        Placement { base: 0x7_7654_3210, targets: [0x7_7000_0000, 0x7_2000_1000, 0x7_7654_4000] },
    ]
}

#[test]
fn two_placements_clear_to_the_stored_bytes() {
    for function in [aarch64(), x86_64()] {
        check(&function.code, function.relocs.view(), In::Code).unwrap();
        let [a, b] = placements();
        let (mut at_a, mut at_b) = (place(&function, &a), place(&function, &b));
        assert_ne!(at_a, at_b, "two placements patch two images");
        assert_ne!(at_a, function.code, "a placement writes the fields");
        assert!(matches!(
            check(&at_a, function.relocs.view(), In::Code),
            Err(FieldError::Nonzero { .. })
        ));
        clear(&mut at_a, function.relocs.view(), In::Code).unwrap();
        clear(&mut at_b, function.relocs.view(), In::Code).unwrap();
        assert_eq!(at_a, function.code);
        assert_eq!(at_b, function.code);
    }
}

#[test]
fn any_placement_clears_to_the_stored_bytes() {
    blaze_sim::check(Config::cases(256), |tape| {
        let function = aarch64();
        let base = tape.int_in(0u64..=1 << 40, "base") & !3;
        let near = |label| base.wrapping_add_signed(tape.int_in(-(1i64 << 26)..=1 << 26, label) & !3);
        let placed = Placement { base, targets: [near("callee"), near("static"), near("got")] };
        let mut code = place(&function, &placed);
        clear(&mut code, function.relocs.view(), In::Code).unwrap();
        assert_eq!(code, function.code);
    });
}

#[test]
fn relocations_round_trip_through_the_blob() {
    for function in [aarch64(), x86_64()] {
        let refs = function.relocs.targets().to_vec();
        let template = Template::of(&function, &refs, &[]).unwrap();
        let back = blob::decode(&blob::encode(&template)).unwrap();
        assert_eq!(back, template);
        let again = back.instantiate(function.key, &refs).unwrap();
        assert_eq!(again.relocs, function.relocs);
        assert_eq!(again.code, function.code);
        check(&again.code, again.relocs.view(), In::Code).unwrap();
    }
}

#[test]
fn a_field_holding_bits_is_refused() {
    let mut function = aarch64();
    // `add x0, x0, #1`: an offset left in the bytes.
    function.code[8..12].copy_from_slice(&0x9100_0400u32.to_le_bytes());
    assert_eq!(
        check(&function.code, function.relocs.view(), In::Code),
        Err(FieldError::Nonzero { row: 2, at: 8, kind: RelocKind::PageOffset12, bits: 0x400 })
    );
    // Bits outside the field are the instruction's: `add x1, x0, #0` is fine.
    function.code[8..12].copy_from_slice(&0x9100_0001u32.to_le_bytes());
    check(&function.code, function.relocs.view(), In::Code).unwrap();
}

#[test]
fn overlapping_and_overlong_fields_are_refused() {
    let target = Target::Function(key(9));
    let at = |at, kind| Relocation { at, kind, target: target.clone(), addend: 0 };
    let both: Relocs = vec![at(0, RelocKind::Abs64), at(4, RelocKind::PcRel32)].into();
    assert_eq!(
        check(&[0; 16], both.view(), In::Code),
        Err(FieldError::Overlap { row: 1, at: 4, earlier: 0 })
    );
    let long: Relocs = vec![at(4, RelocKind::Abs64)].into();
    assert_eq!(
        check(&[0; 8], long.view(), In::Data),
        Err(FieldError::PastEnd { row: 0, at: 4, kind: RelocKind::Abs64, len: 8 })
    );
    let mut bytes = [0xff; 8];
    assert!(clear(&mut bytes, long.view(), In::Data).is_err());
    assert_eq!(bytes, [0xff; 8], "a refused clear writes nothing");
}

#[test]
fn a_wasm_operand_keeps_its_continuation_bits() {
    let target = Target::Function(key(9));
    let call: Relocs =
        vec![Relocation { at: 1, kind: RelocKind::WasmFunction, target, addend: 0 }].into();
    // `call` with a padded LEB of five bytes: 0x80 0x80 0x80 0x80 0x00 is index 0.
    let stored = [0x10, 0x80, 0x80, 0x80, 0x80, 0x00];
    check(&stored, call.view(), In::Code).unwrap();
    let mut placed = [0x10, 0x85, 0x81, 0x80, 0x80, 0x00];
    clear(&mut placed, call.view(), In::Code).unwrap();
    assert_eq!(placed, stored);
}
