//! A unit's panic strategy (panic.rs): the profile's on the target side, the shelf's cut on the
//! macro side and for a build script, `unwind` for a test or bench unit (Cargo's profiles
//! reference, profiles.html#panic: "Tests, benchmarks, build scripts, and proc macros ignore the
//! `panic` setting").
//!
//! Expected: `test panic::tests::... ok` for all four. CONTROL: make `unit_panic` ignore the
//! side and the kind (`Ok(program)` for every unit): `a_proc_macro_keeps_its_cut` fails with
//! "a proc macro's crate takes its shelf row's value, not the profile's".

use blaze_manifest::layout::panic::ProfilePanic;
use blaze_unit_graph::UnitKind;

use super::{Cuts, UnitPanicError, row_panic, unit_panic};
use crate::Side;

/// The real wasm32-wasip1 cut says `panic = "abort"`; the host's run target says `unwind`.
const CUTS: Cuts<'static> = Cuts { macro_side: Some(ProfilePanic::Abort), script_run: ProfilePanic::Unwind, script_triple: "aarch64-apple-darwin" };

#[test]
fn an_abort_profile_aborts_the_target_side() {
    let bin = blaze_own::String::from_str_in("main", blaze_own::testing::heap().alloc());
    for kind in [UnitKind::Lib, UnitKind::Bin(bin)] {
        assert_eq!(unit_panic(ProfilePanic::Abort, Side::Target, &kind, &CUTS), Ok(ProfilePanic::Abort), "workspace target-side crates take the profile's abort");
    }
    assert_eq!(unit_panic(ProfilePanic::Unwind, Side::Target, &UnitKind::Lib, &CUTS), Ok(ProfilePanic::Unwind));
}

#[test]
fn a_proc_macro_keeps_its_cut() {
    // The profile unwinds, the macro side's cut aborts: a function that ignores the side
    // answers the profile's and fails here.
    for (side, kind) in [(Side::Target, UnitKind::ProcMacro), (Side::Macro, UnitKind::ProcMacro), (Side::Macro, UnitKind::Lib)] {
        assert_eq!(unit_panic(ProfilePanic::Unwind, side, &kind, &CUTS), Ok(ProfilePanic::Abort), "a proc macro's crate takes its shelf row's value, not the profile's");
    }
    let unwound = Cuts { macro_side: Some(ProfilePanic::Unwind), ..CUTS };
    assert_eq!(unit_panic(ProfilePanic::Abort, Side::Macro, &UnitKind::Lib, &unwound), Ok(ProfilePanic::Unwind), "nor the profile's abort");
    let none = Cuts { macro_side: None, ..CUTS };
    assert_eq!(unit_panic(ProfilePanic::Abort, Side::Macro, &UnitKind::Lib, &none), Err(UnitPanicError::NoCut { triple: "wasm32-wasip1" }));
}

#[test]
fn tests_and_build_scripts_ignore_the_profile() {
    let test = UnitKind::Test { bin: None };
    assert_eq!(unit_panic(ProfilePanic::Abort, Side::Target, &test, &CUTS), Ok(ProfilePanic::Unwind), "a test unit unwinds whatever the profile says");
    let aborting_run = Cuts { script_run: ProfilePanic::Abort, ..CUTS };
    assert_eq!(unit_panic(ProfilePanic::Unwind, Side::Target, &UnitKind::BuildScript, &aborting_run), Ok(ProfilePanic::Abort), "a build script takes its run target's cut");
    assert_eq!(unit_panic(ProfilePanic::Abort, Side::Target, &UnitKind::BuildScript, &CUTS), Ok(ProfilePanic::Unwind));
}

#[test]
fn a_cut_row_is_read_by_name() {
    let wasm = [("panic", Some("abort")), ("target_arch", Some("wasm32")), ("target_os", Some("wasi"))];
    assert_eq!(row_panic("wasm32-wasip1", wasm.into_iter()), Ok(ProfilePanic::Abort));
    assert_eq!(row_panic("t", [("target_os", Some("macos"))].into_iter()), Err(UnitPanicError::NoPanicAtom { triple: "t" }));
    assert_eq!(row_panic("t", [("panic", Some("immediate-abort"))].into_iter()), Err(UnitPanicError::Value { triple: "t", written: Some("immediate-abort") }));
}
