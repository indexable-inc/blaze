//! The C entry shim (`blaze_build::start::c_main_shim`) starts the program through
//! `lang_start`: it passes `argc` sign-extended to `isize`, `sigpipe::DEFAULT` (0), and
//! returns `lang_start`'s status truncated to `i32`.
//!
//! Rust's rule. The Rust Reference, "Crates and source files", `[crate.main]`: "If a `main`
//! function is present, it must take no arguments ... and its return type must implement the
//! `Termination` trait." std's rt.rs (nightly-2026-10-27, line 198): `#[lang = "start"] fn
//! lang_start<T: crate::process::Termination + 'static>(main: fn() -> T, argc: isize, argv:
//! *const *const u8, sigpipe: u8) -> isize`; std's sys/pal/unix/mod.rs: `pub const DEFAULT: u8
//! = 0;`. The Reference, "Type cast expressions": a smaller signed integer cast to a larger one
//! sign-extends; a larger integer cast to a smaller one truncates. So the C entry
//! `main(argc, argv)` must hand `lang_start` `argc as isize` and `0`, and return its status
//! `as i32`.
//!
//! The test: the shim for a `main` returning `()`, run in LIR's evaluator with a stub
//! `lang_start` that returns 7 exactly when it was given `argc == -2` as an `isize` and
//! `sigpipe == 0`, and 1 otherwise. `argc = -2` is chosen so a shim that zero-extends (giving
//! `0xFFFF_FFFE`) or passes the wrong sigpipe is told apart; a shim that drops the result, or
//! returns its own constant, gives no 7.
//!
//! Expected: `test blaze-build: exit 0; 1 passed, 0 failed` (filter c_main_shim). It needs the
//! evaluator's `func_addr` (LIR EVALUATOR writer 2's program-data piece, wiring
//! blaze-lir/eval-data-1.md): before that lands the run is refused at the shim's `func_addr`.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_build::start::{SIGPIPE_DEFAULT, c_main_shim};
use blaze_lir::builder::{Param, Variadic};
use blaze_lir::body::Target;
use blaze_lir::eval::{Bodies, BodyId, Outcome, Val, run_calls_in};
use blaze_lir::func::{BlockKind, Conv, Ext, RefId, RefKind};
use blaze_lir::{Builder, Flags, Full, Func, IntCond, Ty, View, verify};

/// Far past what the run takes: it may not end by fuel.
const FUEL: u64 = 10_000;
const SHIM: BodyId = BodyId([1; 32]);
const LANG_START: BodyId = BodyId([2; 32]);
const MAIN: BodyId = BodyId([3; 32]);
/// The shim's references: `lang_start::<()>` at place 0, `main` at place 1.
const LANG_START_PLACE: u32 = 0;
const MAIN_PLACE: u32 = 1;
/// `argc` as the C runtime would pass it, and what `lang_start` must see: `-2` as `i32` bits,
/// and `-2` as `isize` (64-bit) bits.
const ARGC_I32: u128 = 0xFFFF_FFFE;
const ARGC_ISIZE: u128 = 0xFFFF_FFFF_FFFF_FFFE;

/// The stub `lang_start::<()>`: `(main: ptr, argc: i64, argv: ptr, sigpipe: i8 zext) -> i64`,
/// returning 7 when `argc == -2` and `sigpipe == 0`, else 1.
fn stub_lang_start() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::PTR), Param { ext: Ext::Zext, ..Param::of(Ty::I8) }],
        &[Param::of(Ty::I64)],
    );
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let _main = b.param(Ty::PTR);
    let argc = b.param(Ty::I64);
    let _argv = b.param(Ty::PTR);
    let sigpipe = b.param(Ty::I8);
    let want_argc = b.iconst(Ty::I64, ARGC_ISIZE);
    let want_sigpipe = b.iconst(Ty::I8, u128::from(SIGPIPE_DEFAULT));
    let argc_ok = b.icmp(IntCond::Eq, argc, want_argc);
    let sigpipe_ok = b.icmp(IntCond::Eq, sigpipe, want_sigpipe);
    let seven = b.iconst(Ty::I64, 7);
    let one = b.iconst(Ty::I64, 1);
    let if_sigpipe = b.select(Flags::NONE, sigpipe_ok, seven, one);
    let status = b.select(Flags::NONE, argc_ok, if_sigpipe, one);
    b.ret(&[status]);
    b.finish_checked().expect("the stub lang_start builds")
}

/// The root's `fn main() {}`: no parameters, no result.
fn unit_main() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    b.ret(&[]);
    b.finish_checked().expect("main builds")
}

/// The shim calls `lang_start` by reference 0 and takes `main`'s address by reference 1.
struct Program {
    lang_start: Func<Global>,
    main: Func<Global>,
}

impl Bodies for Program {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        match (from, reference) {
            (SHIM, RefId(0)) => Some((LANG_START, self.lang_start.view())),
            (SHIM, RefId(1)) => Some((MAIN, self.main.view())),
            _ => None,
        }
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        match (from, reference) {
            (SHIM, RefId(0)) => Some(Target::Func(LANG_START)),
            (SHIM, RefId(1)) => Some(Target::Func(MAIN)),
            _ => None,
        }
    }

    fn body(&self, id: BodyId) -> Option<View<'_>> {
        match id {
            LANG_START => Some(self.lang_start.view()),
            MAIN => Some(self.main.view()),
            _ => None,
        }
    }
}

#[test]
fn c_main_shim_passes_argc_and_sigpipe_and_returns_lang_starts_status() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let lang_start = b.reference(RefKind::Func, LANG_START_PLACE);
    let main = b.reference(RefKind::Func, MAIN_PLACE);
    assert_eq!((lang_start, main), (RefId(0), RefId(1)), "the references are rows 0 and 1");
    c_main_shim(&mut b, lang_start, main).expect("the shim builds");
    let shim = b.finish_checked().expect("the shim verifies");
    let program = Program { lang_start: stub_lang_start(), main: unit_main() };
    for view in [shim.view(), program.lang_start.view(), program.main.view()] {
        verify(view).expect("every body of the run verifies");
    }
    let argv_null = Val::Ptr { slot: Val::NO_SLOT, offset: 0 };
    let outcome = run_calls_in(shim.view(), SHIM, &[Val::Int(ARGC_I32), argv_null], FUEL, &program, Global)
        .expect("the evaluator runs the shim");
    match outcome {
        Outcome::Returned(values) => {
            assert_eq!(&values[..], &[Val::Int(7)], "the C main returns lang_start's status, truncated to i32")
        }
        other => panic!("the shim did not return: {other:?}"),
    }
}
