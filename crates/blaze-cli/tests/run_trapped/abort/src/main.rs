// A program that dies on a trap on purpose (run_trapped.rs
// `a_trap_is_said_by_the_function_s_path_and_its_reason`): `main` calls `give_up`, which calls
// the `abort_immediate` intrinsic, built in place as `brk #0xfe00` (blaze-code trap.rs
// `ABORT`). The program ends by SIGTRAP; `blaze run` names `give_up` by its path and the trap
// by its reason.
#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = "pointee_sized"]
pub trait PointeeSized {}

#[lang = "meta_sized"]
pub trait MetaSized: PointeeSized {}

#[lang = "sized"]
pub trait Sized: MetaSized {}

#[lang = "copy"]
pub trait Copy {}

#[rustc_intrinsic]
pub fn abort_immediate() -> !;

#[inline(never)]
fn give_up() -> ! {
    abort_immediate()
}

// `extern "C-unwind"`: an `extern "C"` fn that calls a fn which may unwind needs the
// `panic_cannot_unwind` lang item for its abort (the Reference, [items.fn.extern.abort]), which
// this program does not declare (blaze refuses it by name, as rustc does).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    give_up()
}
