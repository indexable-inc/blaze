//! The record form's unwind rows (pass/unwind.rs): an `invoke` gets a cleanup row covering its
//! `blr` and landing at its landing block, whose first instruction stores `x0` (the exception
//! pointer) to the block's parameter; a plain call gets a row that lands nowhere; a call inside
//! the cleanup gets none; `resume` calls `_Unwind_Resume`, whose call gets a row that lands
//! nowhere. The literal-word form refuses it.

use std::alloc::Global;

use blaze_code::record::CodeView;
use blaze_code::unwind::{self, Action, Frame};
use blaze_lir::{
    Builder, Flags, Full, Ty,
    builder::{Param, Variadic},
    func::{BlockKind, Conv, Func, RefKind},
};
use blaze_own::{Alloc, Vec};

use crate::{Named, Names, Outside, record::emit_record, tests::{ISA, emitted}};

const BLR_X16: u32 = 0xd63f_0200;

struct ByPlace;

impl Names for ByPlace {
    fn name(&self, of: Outside) -> Option<Named<'_>> {
        Some(Named::Symbol(blaze_code::record::Symbol::of_name(&format!("{of:?}"))))
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// `plain(); invoke may_panic() to normal unwind pad; normal: ret; pad(e): dropper(e); resume e`.
fn with_cleanup() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let none = b.sig(Conv::C, Variadic::No, &[], &[]);
    let one_ptr = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
    let may_panic = b.reference(RefKind::Func, 0);
    let dropper = b.reference(RefKind::Func, 1);
    let plain = b.reference(RefKind::Func, 2);
    let entry = b.create_block(BlockKind::Normal);
    let normal = b.create_block(BlockKind::Normal);
    let pad = b.create_block(BlockKind::Cleanup);
    b.begin(entry);
    b.call(Flags::NONE, plain, none, &[]);
    b.invoke(Flags::NONE, may_panic, none, &[], normal, pad);
    b.begin(normal);
    b.ret(&[]);
    b.begin(pad);
    let exception = b.param(Ty::PTR);
    b.call(Flags::NONE, dropper, one_ptr, &[exception]);
    b.resume(exception);
    b.finish_checked().expect("builds")
}

fn inst(code: &[u8], at: u32) -> u32 {
    let a = at as usize;
    u32::from_le_bytes([code[a], code[a + 1], code[a + 2], code[a + 3]])
}

/// Prints `unwind rows: 3 rows (continue, cleanup, continue) for 4 calls; the landing stores x0 first`.
#[test]
fn an_invoke_gets_a_cleanup_row_and_a_call_in_the_cleanup_none() {
    let func = with_cleanup();
    let (record, _) = emit_record(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &ByPlace, &crate::RoutinePlaces::NONE, ISA, Alloc::global()).expect("emits");
    let sealed = record.seal().expect("seals");
    let mut bytes = Vec::new_in(Alloc::global());
    sealed.encode(&mut bytes);
    let view = CodeView::read(&bytes).unwrap();
    let resume = blaze_code::record::Symbol::of_name("_Unwind_Resume");
    assert_eq!(view.name(resume), Some("_Unwind_Resume"), "resume calls `_Unwind_Resume`");
    let rows = unwind::read(view).expect("the rows are well formed");
    assert_eq!(rows.frame(), Some(Frame::PLAIN));
    let code = view.code();
    let blrs = (0..code.len() as u32 / 4).filter(|&i| inst(code, 4 * i) == BLR_X16).count();
    assert_eq!(blrs, 4, "plain, may_panic, dropper, _Unwind_Resume");
    assert_eq!(rows.rows(), 3, "plain's, may_panic's and _Unwind_Resume's");
    let (first, second, third) = (rows.row(0), rows.row(1), rows.row(2));
    assert_eq!((first.action, first.landing, first.len), (Action::Continue, 0, 4));
    assert_eq!((second.action, second.len), (Action::Cleanup, 4));
    assert_eq!(inst(code, first.start), BLR_X16);
    assert_eq!(inst(code, second.start), BLR_X16);
    // `_Unwind_Resume`'s `blr`, the last, continues though it is in the cleanup: phase 2 asks
    // this frame of it, and with no row the process aborts ("_Unwind_Resume() can't return").
    let last = (0..code.len() as u32 / 4).rev().find(|&i| inst(code, 4 * i) == BLR_X16).unwrap();
    assert_eq!((third.action, third.landing, third.len, third.start), (Action::Continue, 0, 4, 4 * last), "resume's row");
    // `sp` put back from `x29` first (pass/stack.rs: an `invoke` with stacked arguments lands
    // with `sp` lowered): `sub x16, x29, #hi, lsl #12; sub x16, x16, #lo; mov sp, x16`.
    assert_eq!(inst(code, second.landing) & 0xffc0_03ff, 0xd140_03b0, "the landing's first instruction");
    assert_eq!(inst(code, second.landing + 4) & 0xffc0_03ff, 0xd100_0210, "its second");
    assert_eq!(inst(code, second.landing + 8), 0x9100_021f, "`mov sp, x16`");
    // Then `str x0, [sp, #imm]`: the exception pointer to the landing block's parameter cell.
    assert_eq!(inst(code, second.landing + 12) & 0xffc0_03ff, 0xf900_03e0, "the landing's fourth instruction");
    // The dropper's `blr`, inside the cleanup, has no row: an unwind out of it ends the program.
    let in_cleanup = (second.landing / 4..code.len() as u32 / 4).find(|&i| inst(code, 4 * i) == BLR_X16).unwrap();
    assert_ne!(in_cleanup, last, "the dropper's call is before resume's");
    assert!((0..rows.rows()).all(|i| rows.row(i).start != 4 * in_cleanup));
    println!("unwind rows: {} rows (continue, cleanup, continue) for {blrs} calls; the landing stores x0 first", rows.rows());
}

/// Prints `unwind rows: the literal-word form refuses: ..`.
#[test]
fn the_literal_word_form_refuses_unwinding() {
    let func = with_cleanup();
    let Err(error) = emitted(func.view(), &[crate::Via::Address, crate::Via::Address, crate::Via::Address]) else {
        panic!("the literal-word form emitted a landing block");
    };
    let text = format!("{error}");
    assert!(text.contains("unwinding in the literal-word form"), "{text}");
    println!("unwind rows: the literal-word form refuses: {text}");
}
