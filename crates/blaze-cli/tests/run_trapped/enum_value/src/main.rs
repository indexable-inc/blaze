// A program that holds an enum value no variant has, on purpose (run_trapped_enum.rs
// `a_bad_discriminant_is_said_by_its_value_and_the_enum`): `make`, under `build`, `run` and
// `main`, transmutes 3, computed at
// run time, into `Two` (`#[repr(usize)]`, discriminants 1 and 2) inside a struct shaped like
// core's `Layout` (a `usize`, then the enum), and passes the struct by value. The copy's switch
// on the discriminant (blaze-build walk.rs `copy`) matches no case and reaches its
// `unreachable`: the program ends by SIGTRAP, and `blaze run` says the value it read (0x3, from
// x9), names `Two` and lists its discriminants 0x1 and 0x2. Holding such a value is UB; the
// program exists to make the trap.
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
pub unsafe fn transmute<Src, Dst>(src: Src) -> Dst;

#[repr(usize)]
pub enum Two {
    One = 1,
    Other = 2,
}

/// `core::alloc::Layout`'s shape: a size, then the enum.
pub struct L {
    pub size: usize,
    pub align: Two,
}

/// 3, out of the compiler's sight.
#[inline(never)]
fn three() -> usize {
    3
}

#[inline(never)]
unsafe fn make(size: usize, align: usize) -> L {
    L { size, align: unsafe { transmute::<usize, Two>(align) } }
}

#[inline(never)]
fn pass(l: L) -> L {
    l
}

/// `make` one call below `run`, as core's `Layout::from_size_align_unchecked` is below its
/// callers: the trap is two calls or more under `main`, so the line names its callers
/// (run_trapped_enum.rs asserts `run` and `main` among them).
#[inline(never)]
fn build(size: usize, align: usize) -> L {
    unsafe { make(size, align) }
}

#[inline(never)]
fn run() -> i32 {
    let l = pass(build(8, three()));
    match l.align {
        Two::One => 1,
        Two::Other => 2,
    }
}

// `extern "C-unwind"`, as run_trapped/abort's `main` is and for its reason: no
// `panic_cannot_unwind` lang item here.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    run()
}
