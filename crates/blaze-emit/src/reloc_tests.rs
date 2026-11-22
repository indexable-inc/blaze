//! Absolute address words ([`crate::Reloc`]): a static's address reached through a literal
//! word, and data rows holding the addresses of other data rows, statics and functions. The
//! fill rule tested is the one placement follows: the word at `at` becomes the address of `to`
//! plus `addend`, little-endian; `Code(off)` is the code's first byte plus `off`.

use std::alloc::Global;

use blaze_lir::{
    Builder, Flags, Full, Op, Ty,
    builder::{DataReloc, MemAttrs, Param, Variadic},
    func::{BlockKind, Conv, Func, RefKind},
};

use crate::{Code, RelocTo, Via, emit, tests::ISA};

/// `f() = *(*table) + STATIC` after `STATIC += 2`, where `table` is a data row holding the
/// address of `text` plus 1, and `STATIC` is static place 0.
pub(crate) fn reader() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let text = b.data(b"hi!", 0, &[]);
    let table = b.data(&[0; 8], 3, &[DataReloc { offset: 0, target: text, addend: 1 }]);
    let stat = b.reference(RefKind::Static, 0);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let s = b.data_addr(stat);
    let old = b.load(Ty::I64, Flags::NONE, s, 0, MemAttrs::aligned(3));
    let two = b.iconst(Ty::I64, 2);
    let new = b.binary(Op::Add, Flags::NONE, old, two);
    b.store(Flags::NONE, new, s, 0, MemAttrs::aligned(3));
    let t = b.data_addr(table);
    let p = b.load(Ty::PTR, Flags::NONE, t, 0, MemAttrs::aligned(3));
    let byte = b.load(Ty::I8, Flags::NONE, p, 0, MemAttrs::default());
    let wide = b.convert(Op::Zext, Flags::NONE, Ty::I64, byte);
    let again = b.load(Ty::I64, Flags::NONE, s, 0, MemAttrs::aligned(3));
    let sum = b.binary(Op::Add, Flags::NONE, wide, again);
    b.ret(&[sum]);
    b.finish_checked().expect("builds")
}

#[test]
fn relocations_name_the_static_and_the_inner_data() {
    let func = reader();
    let mut code = Code::new_in(Global);
    emit(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &[], ISA, &mut code).expect("emits");
    assert!(code.fixups.is_empty(), "no function is called");
    let statics: Vec<_> = code.relocs.iter().filter(|r| r.to == RelocTo::Static(0)).collect();
    assert_eq!(statics.len(), 1, "one literal word for the static, used twice");
    assert_eq!(statics[0].at % 8, 0, "a literal word is 8-aligned");
    let inner: Vec<_> = code.relocs.iter().filter(|r| matches!(r.to, RelocTo::Code(_))).collect();
    assert_eq!(inner.len(), 1);
    let RelocTo::Code(text_at) = inner[0].to else { unreachable!() };
    assert_eq!(&code.bytes[text_at as usize..text_at as usize + 3], b"hi!", "the reloc reaches the text row");
    assert_eq!(inner[0].addend, 1);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn a_placed_function_reads_and_writes_a_static_and_follows_data_addresses() {
    use blaze_inproc::exec::Executable;

    let func = reader();
    let mut code = Code::new_in(Global);
    emit(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &[], ISA, &mut code).expect("emits");
    let mut cell: Box<u64> = Box::new(40);
    let static_at = &mut *cell as *mut u64 as u64;
    let exec = Executable::written(code.bytes.len(), 0, |out, _| {
        let base = out.as_ptr() as u64;
        out.copy_from_slice(&code.bytes);
        for r in code.relocs.iter() {
            let target = match r.to {
                RelocTo::Static(0) => static_at,
                RelocTo::Code(off) => base + u64::from(off),
                other => panic!("an unexpected relocation {other:?}"),
            };
            let at = r.at as usize;
            out[at..at + 8].copy_from_slice(&target.wrapping_add_signed(r.addend).to_le_bytes());
        }
    })
    .expect("the code maps");
    // SAFETY: the code is an emitted C function of no parameters returning an i64, its words
    // filled with the address of `cell` (alive for the call) and of its own data.
    let f = unsafe { std::mem::transmute::<usize, extern "C" fn() -> u64>(exec.code_address() as usize) };
    assert_eq!(f(), u64::from(b'i') + 42, "`i` (the text plus 1) plus the static after += 2");
    assert_eq!(*cell, 42, "the store reached the static");
    assert_eq!(f(), u64::from(b'i') + 44);
}

/// A function pointer in data to a slot function is its slot stub's address; to an outside
/// function, its own.
#[test]
fn a_slot_function_address_in_data_is_its_slot_stub() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
    b.set_signature(sig);
    let callee = b.reference(RefKind::Func, 0);
    let table = b.data(&[0; 8], 3, &[DataReloc { offset: 0, target: callee, addend: 0 }]);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let t = b.data_addr(table);
    b.ret(&[t]);
    let func = b.finish_checked().expect("builds");
    let mut code = Code::new_in(Global);
    let verified = blaze_lir::verify(func.view()).expect("the test's function is LIR");
    emit(verified, &[Via::Slot], ISA, &mut code).expect("a slot function's address is placed");
    assert_eq!(code.relocs.len(), 1);
    assert_eq!(code.relocs[0].to, RelocTo::SlotStub(0));
    emit(verified, &[Via::Address], ISA, &mut code).expect("an outside function's address is placed");
    assert_eq!(code.relocs.len(), 1);
    assert_eq!(code.relocs[0].to, RelocTo::Func(0));
}

/// `func_addr` of a slot function loads a literal word placement fills with its slot stub's
/// address (the stub itself is placement's); run, it returns that word.
#[cfg(target_arch = "aarch64")]
#[test]
fn func_addr_of_a_slot_function_is_its_stub() {
    use crate::tests::{emitted, run};

    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
    b.set_signature(sig);
    let callee = b.reference(RefKind::Func, 0);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let at = b.func_addr(callee);
    b.ret(&[at]);
    let func = b.finish_checked().expect("builds");
    let code = emitted(func.view(), &[Via::Slot]).expect("emits");
    assert!(code.fixups.is_empty(), "no slot literal: the stub is the address");
    assert_eq!(code.relocs.iter().map(|r| r.to).collect::<Vec<_>>(), [RelocTo::SlotStub(0)]);
    let exec = run::place_all(&code, |_| unreachable!(), |to| match to {
        RelocTo::SlotStub(0) => 0x1234_5678,
        other => panic!("{other:?}"),
    });
    assert_eq!(run::call(&exec, [0; 4]), 0x1234_5678);
}
