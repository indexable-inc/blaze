//! The pointer rows of the Reference's cast table, BUILT AND RUN on the new front end
//! (blaze-build walk/casts.rs). Found on tempo's path (49 tests NOT RUN): alloc's
//! `box_assume_init_into_vec_unsafe` (library/alloc/src/boxed.rs:346), which `vec![..]`
//! expands to, `unsafe { (b.assume_init() as Box<[T]>).into_vec() }`, was refused with "this
//! cast (a wide pointer to an integer, or an aggregate other than an enum) is not built to LIR
//! yet": the operand is a `Box<[T; N]>` the checker coerced to `Box<[T]>` (a trivial cast), an
//! ADT, which `cast_other` had no row for.
//!
//! The Reference, "Type cast expressions": "`e` has type `T` and `T` coerces to `U`:
//! Coercion-cast"; "`*T` where `T: ?Sized` -> `*V` where `V: ?Sized`: Pointer to pointer
//! cast" (wide to wide: "`*const [T]` -> `*const [U]` ... the length is kept"); "`*T` where
//! `T: Sized` -> Integer type: Pointer to address cast" (a THIN pointer only: a wide one is
//! rustc's E0606). "Coercion types": "TyCtor(`T`) to TyCtor(`U`), where TyCtor(`T`) is one of
//! ... and `U` is obtained from `T` by unsized coercion": `CoerceUnsized` on a struct whose
//! field is a pointer (alloc's `Box`, `Unique`, `NonNull`). "Destructors": a slice's elements
//! drop first to last.
//!
//! The library is the twins' prelude (crates/blaze-cli/tests/ladder/front2/twin-prelude.rs),
//! with a box of this program's own, `Bx<T: ?Sized>(*mut T)`, unsizing as alloc's `Box` does.
//! The programs are `#![no_core]`, built and run in this process with no toolchain.

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/twin-prelude.rs");

const HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<*mut U> for *mut T {}

/// alloc's `Box` in small: one pointer field, unsizing by `CoerceUnsized`.
struct Bx<T: ?Sized>(*mut T);

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Bx<U>> for Bx<T> {}

static mut LOG: u32 = 0;

fn log(id: u32) {
    unsafe { LOG = LOG * 10 + id }
}

struct D(u32);

impl Drop for D {
    fn drop(&mut self) {
        log(self.0);
    }
}

/// The contents' drop, as alloc's box glue runs it (no free: the storage is the caller's).
struct Owning<T: ?Sized>(*mut T);

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Owning<U>> for Owning<T> {}

impl<T: ?Sized> Drop for Owning<T> {
    fn drop(&mut self) {
        unsafe { drop_in_place(self.0) }
    }
}

/// `s[0] + 10 * s[1] + 20 * s[2]` through the box's slice (the length is read by each
/// index's bounds check).
fn weigh(b: Bx<[u32]>) -> u32 {
    let s: &[u32] = unsafe { &*b.0 };
    s[0] + s[1] * 10 + s[2] * 20
}
";

/// Builds and runs `HEAD` + `main` with the twins' prelude: the status, or the refusal on one
/// line.
fn run(main: &str) -> Result<u8, String> {
    let text = [HEAD, main].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// The coercion cast of a box, `vec!`'s form: `Bx<[u32; 3]> as Bx<[u32]>`, the adjusted
/// operand of the cast's own type, so the cast is its value: the data pointer and the length
/// 3. `weigh` = 1 + 20 + 60 = 81. THE CONTROL: the same through a raw pointer's coercion
/// cast (`*mut [u32; 3] as *mut [u32]`, a pointer node, built before the fix) = 81 too.
/// Before the fix the box form failed the build with "this cast (a wide pointer to an
/// integer, or an aggregate other than an enum)".
#[test]
fn a_box_coercion_cast_keeps_the_length() {
    let boxed = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut a = [1u32, 2, 3];
    let b: Bx<[u32; 3]> = Bx(&mut a as *mut [u32; 3]);
    let s = b as Bx<[u32]>;
    weigh(s) as i32
}
");
    println!("BOX COERCION CAST: {boxed:?}");
    assert_eq!(boxed, Ok(81));
    let raw = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut a = [1u32, 2, 3];
    let p = &mut a as *mut [u32; 3] as *mut [u32];
    weigh(Bx(p)) as i32
}
");
    println!("RAW COERCION CAST: {raw:?}");
    assert_eq!(raw, Ok(81));
}

/// The box's slice dropped through the cast's value: `Owning<[D; 2]> as Owning<[D]>`, its
/// `Drop` dropping `*mut [D]` by the slice's glue, which reads the kept length 2: `D(1)` then
/// `D(2)`, LOG 12 when read after the box's scope and before `a`'s. A length lost (0) gives 0;
/// a length past 2 reads past the array. (`a` drops its elements again after the return value
/// is taken: the storage is this test's, not the box's.)
#[test]
fn a_cast_boxs_slice_drops_each_element_once() {
    let got = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut a = [D(1), D(2)];
    {
        let b: Owning<[D; 2]> = Owning(&mut a as *mut [D; 2]);
        let _s = b as Owning<[D]>;
    }
    let got = unsafe { LOG };
    got as i32
}
");
    println!("CAST BOX SLICE DROPS: {got:?}");
    assert_eq!(got, Ok(12));
}

/// THE NEGATIVE TWIN, a cast no row of the table holds: a wide pointer to an integer
/// (`*const [u32] as usize`, rustc's E0606, "cast through a thin pointer first"). The checker
/// does not refuse it yet (blaze-typeck rules/cast.rs, "Omitted for now: the metadata rules
/// of wide-pointer casts"), so the build must, by name, never building the address alone or
/// both words. The twin through a thin pointer (`as *const u32 as usize`) is a row and runs:
/// the address's low bits are the array's alignment's, 0 (status 0).
#[test]
fn a_wide_pointer_to_an_integer_is_refused_by_name() {
    let wide = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = [1u32, 2, 3];
    let p = &a as *const [u32; 3] as *const [u32];
    let n = p as usize;
    (n & 0) as i32
}
");
    println!("WIDE TO INT: {wide:?}");
    let Err(error) = wide else { panic!("a wide pointer cast to an integer must be refused: {wide:?}") };
    assert!(error.contains("a wide pointer cast to an integer") || error.contains("E0606"), "{error}");
    let thin = run("
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = [1u32, 2, 3];
    let p = &a as *const [u32; 3] as *const [u32];
    let n = p as *const u32 as usize;
    (n & 3) as i32
}
");
    println!("WIDE TO THIN TO INT: {thin:?}");
    assert_eq!(thin, Ok(0));
}
