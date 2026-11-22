//! The allocator shim's call, imitated (RF12 piece 5, third round). std hello's trap: core's
//! `from_size_align_unchecked` got a STACK ADDRESS as `align`, called from std's `__rdl_alloc`
//! +0x88, from `alloc::alloc::alloc` +0x940. The pinned texts:
//!
//! ```text
//! // alloc/src/alloc.rs:12-22, 151-158
//! unsafe extern "Rust" { .. fn __rust_alloc(size: usize, align: Alignment) -> *mut u8; .. }
//! pub unsafe fn alloc(layout: Layout) -> *mut u8 {
//!     unsafe { __rust_no_alloc_shim_is_unstable_v2(); __rust_alloc(layout.size(), layout.alignment()) }
//! }
//! // std/src/alloc.rs:473-474
//! #[rustc_std_internal_symbol]
//! pub unsafe extern "C" fn __rdl_alloc(size: usize, align: usize) -> *mut u8 { .. }
//! ```
//!
//! The declaration is "Rust" and takes `Alignment`, an aggregate (a `#[repr(transparent)]`
//! struct over a `#[repr(usize)]` enum); the default it is bound to (blaze-ladder
//! front2/alloc_shim.rs `bound`, by NAME, as here) is "C" and takes a `usize`. blaze-build's
//! walk lowers the call from `Adts::abi` of the callee item, the DECLARATION (walk.rs `call`:
//! `self.adts.abi(item)`, then `self.pass(g, repr, abi, ..)` per argument): "Rust" passes an
//! aggregate by address (walk.rs `rust_pass`: `Repr::Agg(_) => Pass::Ref`), so `x1` holds the
//! address of the caller's temporary holding the alignment. `__rdl_alloc`'s own body is built
//! from its own "C" head and reads `x1` as the `usize`: the address. The same holds for
//! `NonNull<u8>` (`__rust_dealloc`, `__rust_realloc`'s `ptr`).
//!
//! Each row is one program whose `main` calls an `extern "Rust"` declaration named as alloc's,
//! bound to the one fn of the graph named as std's default, with `(l.size(), l.alignment())` of
//! an `L` passed by pointer (`alloc(layout: L)`, as core's `alloc(layout: Layout)`):
//!
//! - (s1) THE SHAPE: declared `(usize, W)` "Rust", defined `(usize, usize)` "C".
//! - (s2) CONTROL, the ABI difference removed: defined `(usize, W)` "Rust" (as blaze's
//!   `#[global_allocator]` expansion writes its fns: blaze-expand global_allocator.rs).
//! - (s3) CONTROL, the aggregate removed: declared `(usize, usize)` "Rust", defined "C".
//! - (s4) `__rust_realloc`'s shape: a pointer newtype first, `W` third, four parameters.
//!
//! A default returns the alignment it read when its other parameters are right, else a
//! sentinel that is no power of two: 3 a wrong size, 5 a wrong pointer, 7 a wrong new size.
//! `main` returns 250 when every `n` in 0..8 passed, else 1 + n a wrong size, 100 a wrong
//! pointer, 101 a wrong new size, 64 + n a wrong alignment. Printed: "shim abi <row>: walk
//! <counts or refusal>; status <n or refusal>".
//!
//! Before the fix (the call made with the declaration's "Rust"): (s1) 64, (s2) 250, (s3) 250,
//! (s4) 100. After it (made with the definition's ABI, `Adts::abi`'s doc): all four 250.

use super::{MemoryStore, PASSED, alloc, fixture, run_whole, walk_into};

/// The lang items, `Copy` for the types copied out of `&self`, `AlignmentEnum`'s first eight
/// variants as `A8`, `W` as `Alignment` (a named field, as core's `_inner_repr_trick`), `L` as
/// `Layout` with core's two getters, and `pass` (a by-value round trip of an `L`).
macro_rules! head {
    () => {
        "#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features, dead_code, unused_unsafe)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for usize {}
impl Copy for i32 {}
impl Copy for bool {}
impl Copy for A8 {}
impl Copy for W {}

#[repr(usize)]
enum A8 {
    S0 = 1 << 0,
    S1 = 1 << 1,
    S2 = 1 << 2,
    S3 = 1 << 3,
    S4 = 1 << 4,
    S5 = 1 << 5,
    S6 = 1 << 6,
    S7 = 1 << 7,
}

#[repr(transparent)]
struct W {
    inner: A8,
}

struct L {
    size: usize,
    align: W,
}

#[inline(never)]
fn w_of(n: usize) -> W {
    let inner = match n {
        0 => A8::S0,
        1 => A8::S1,
        2 => A8::S2,
        3 => A8::S3,
        4 => A8::S4,
        5 => A8::S5,
        6 => A8::S6,
        _ => A8::S7,
    };
    W { inner }
}

impl L {
    #[inline(never)]
    fn size(&self) -> usize {
        self.size
    }

    #[inline(never)]
    fn alignment(&self) -> W {
        self.align
    }

    #[inline(never)]
    fn align(&self) -> usize {
        self.align.inner as usize
    }
}

#[inline(never)]
fn pass(l: L) -> L {
    l
}
"
    };
}

/// `main`: `call` is the expression of `*mut u8` the row makes of `l`.
macro_rules! main_of {
    ($call:literal) => {
        concat!(
            "
#[unsafe(no_mangle)]
pub extern \"C-unwind\" fn main() -> i32 {
    let mut n: usize = 0;
    while n < 8 {
        let want: usize = 1usize << n;
        let l = pass(L { size: 16, align: w_of(n) });
        let p = unsafe { ",
            $call,
            " } as usize;
        if p == 3 {
            return (1 + n) as i32;
        }
        if p == 5 {
            return 100;
        }
        if p == 7 {
            return 101;
        }
        if p != want {
            return (64 + n) as i32;
        }
        n += 1;
    }
    250
}
"
        )
    };
}

/// (s1): alloc's declaration and std's default, as the pinned texts write them.
const S1: &str = concat!(
    head!(),
    "
unsafe extern \"Rust\" {
    #[rustc_nounwind]
    fn __rust_alloc(size: usize, align: W) -> *mut u8;
}

pub unsafe extern \"C\" fn __rdl_alloc(size: usize, align: usize) -> *mut u8 {
    if size != 16 {
        return 3 as *mut u8;
    }
    align as *mut u8
}

#[inline(never)]
unsafe fn alloc(layout: L) -> *mut u8 {
    unsafe { __rust_alloc(layout.size(), layout.alignment()) }
}
",
    main_of!("alloc(l)")
);

/// (s2): the default takes the declaration's types under the declaration's ABI.
const S2: &str = concat!(
    head!(),
    "
unsafe extern \"Rust\" {
    #[rustc_nounwind]
    fn __rust_alloc(size: usize, align: W) -> *mut u8;
}

pub unsafe fn __rdl_alloc(size: usize, align: W) -> *mut u8 {
    if size != 16 {
        return 3 as *mut u8;
    }
    align.inner as usize as *mut u8
}

#[inline(never)]
unsafe fn alloc(layout: L) -> *mut u8 {
    unsafe { __rust_alloc(layout.size(), layout.alignment()) }
}
",
    main_of!("alloc(l)")
);

/// (s3): two scalars on both sides; only the ABI's name differs.
const S3: &str = concat!(
    head!(),
    "
unsafe extern \"Rust\" {
    #[rustc_nounwind]
    fn __rust_alloc(size: usize, align: usize) -> *mut u8;
}

pub unsafe extern \"C\" fn __rdl_alloc(size: usize, align: usize) -> *mut u8 {
    if size != 16 {
        return 3 as *mut u8;
    }
    align as *mut u8
}

#[inline(never)]
unsafe fn alloc(layout: L) -> *mut u8 {
    unsafe { __rust_alloc(layout.size(), layout.align()) }
}
",
    main_of!("alloc(l)")
);

/// (s4): `__rust_realloc(ptr: NonNull<u8>, old_size, align: Alignment, new_size)` against
/// std's `__rdl_realloc(ptr: *mut u8, old_size, align: usize, new_size)`; `P` stands for
/// `NonNull<u8>` (`#[repr(transparent)]` over a raw pointer).
const S4: &str = concat!(
    head!(),
    "
#[repr(transparent)]
struct P {
    pointer: *const u8,
}

unsafe extern \"Rust\" {
    #[rustc_nounwind]
    fn __rust_realloc(ptr: P, old_size: usize, align: W, new_size: usize) -> *mut u8;
}

pub unsafe extern \"C\" fn __rdl_realloc(ptr: *mut u8, old_size: usize, align: usize, new_size: usize) -> *mut u8 {
    if ptr as usize != 4096 {
        return 5 as *mut u8;
    }
    if old_size != 16 {
        return 3 as *mut u8;
    }
    if new_size != 32 {
        return 7 as *mut u8;
    }
    align as *mut u8
}

#[inline(never)]
unsafe fn realloc(layout: L) -> *mut u8 {
    unsafe { __rust_realloc(P { pointer: 4096 as *const u8 }, layout.size(), layout.alignment(), 32) }
}
",
    main_of!("realloc(l)")
);

const ROWS: [(&str, &str); 4] = [
    ("(s1) Rust declaration (usize, W), C default (usize, usize)", S1),
    ("(s2) control: Rust default (usize, W)", S2),
    ("(s3) control: Rust declaration (usize, usize)", S3),
    ("(s4) realloc: pointer newtype, W third, four parameters", S4),
];

/// Every row walked (the lazy path) and run whole (the eager path), every line printed, then
/// every row asserted to 250 (the statuses are in the module doc).
#[test]
fn shim_abi_declaration_meets_default() {
    let said = ROWS.map(|(name, text)| {
        let f = fixture(text);
        let mut store = MemoryStore::new_in(alloc());
        let walked = walk_into(&f, &mut store);
        let ran = run_whole(text);
        match (&walked, &ran) {
            (Ok(counts), Ok(status)) => println!("shim abi {name}: walk {counts}; status {status}"),
            (Ok(counts), Err(e)) => println!("shim abi {name}: walk {counts}; status REFUSED: {e}"),
            (Err(e), Ok(status)) => println!("shim abi {name}: walk REFUSED: {e}; status {status}"),
            (Err(e), Err(r)) => println!("shim abi {name}: walk REFUSED: {e}; status REFUSED: {r}"),
        }
        (name, text, walked.is_ok(), ran)
    });
    for (name, text, walked, ran) in said {
        assert!(walked, "{name}: the lazy path refused (its line above)");
        match ran {
            Ok(status) => assert_eq!(status, PASSED, "{name}: the statuses are in this module's doc\n{text}"),
            Err(e) => panic!("{name}: the whole run refused: {e}"),
        }
    }
}
