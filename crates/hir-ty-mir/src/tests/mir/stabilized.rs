//! What the language changes of Rust 1.80 to 1.100 do when the program runs: each as a
//! program whose `main` checks itself, run on the MIR evaluator at every optimization level
//! (`check_pass`), so the MIR blaze builds for it means what Rust says: drop order, layout,
//! const evaluation, coercions, patterns.
//!
//! That the same programs are accepted with nothing said is blaze-diagnostics'
//! `tests/stabilized.rs`.

use super::eval::{check_error_with, check_pass_and_stdio};
use crate::mir_pretty::MirEvalErrorPretty as _;

/// A `check` that does not hold panics, which stops the run with an error; that it can is
/// held for every fixture (`tests::assertion_helpers`), and the control below runs it.
const PRE: &str = r#"
fn check(b: bool) { if !b { panic!("a check did not hold") } }
"#;

/// `D(log, n)` appends the digit `n` to `log` when it is dropped, and `log(l, n)` when called.
const LOG: &str = r#"
use core::cell::Cell;
struct D<'a>(&'a Cell<u32>, u32);
impl<'a> Drop for D<'a> { fn drop(&mut self) { self.0.set(self.0.get() * 10 + self.1) } }
fn log(l: &Cell<u32>, n: u32) { l.set(l.get() * 10 + n) }
"#;

/// `source` after [`PRE`], beside minicore with `meta`'s flags and `panic`. `meta` is the
/// `minicore:` line, then any other line of the fixture's head (`//- /main.rs edition:2024`).
fn fixture(meta: &str, source: &str) -> String {
    let (flags, rest) = meta.split_once('\n').unwrap_or((meta, ""));
    let rest = if rest.is_empty() { String::new() } else { format!("{rest}\n") };
    format!("//- {flags}, panic\n{rest}{PRE}{}", source.replace("LOG", LOG))
}

/// `source`'s `main` returns: every `check` in it held.
#[track_caller]
fn runs(meta: &str, source: &str) {
    check_pass_and_stdio(&fixture(meta, source), "", "");
}

/// The control of every test here: a `check` that does not hold stops the run, and one
/// that holds does not.
#[test]
fn the_oracle_catches_a_check_that_does_not_hold() {
    let source = "fn main() { check(1 + 1 == 3); }";
    check_error_with(&fixture("minicore: copy, add, builtin_impls", source), |e| {
        e.is_panic().is_some()
    });
    runs("minicore: copy, add, builtin_impls", "fn main() { check(1 + 1 == 2); }");
}

// Rust 1.100

/// `!` in types: layout (a variant holding a `!` takes no space), irrefutable and
/// exhaustive patterns over it, and the fallback of an unconstrained diverging variable to
/// `!` (in the 2021 edition, where it was `()` before 1.100): `T::tag()` is `!`'s.
#[test]
fn v1_100_never_type_layout_patterns_and_fallback() {
    runs(
        "minicore: result, option, size_of, copy\n//- /main.rs edition:2021",
        r#"
fn irrefutable(x: Result<u32, !>) -> u32 { let Ok(v) = x; v }
fn one_arm(x: Result<u32, !>) -> u32 { match x { Ok(v) => v } }
fn by_ref(x: &Result<u32, !>) -> u32 { match x { Ok(v) => *v, Err(e) => *e } }
fn size<T>(_: &Option<T>) -> usize { core::mem::size_of::<T>() }
trait Tag { fn tag() -> u8; }
impl Tag for () { fn tag() -> u8 { 1 } }
impl Tag for ! { fn tag() -> u8 { 2 } }
fn tag_of<T: Tag>(_: &Option<T>) -> u8 { T::tag() }
type Infallible = !;
fn main() {
    check(core::mem::size_of::<!>() == 0);
    check(core::mem::size_of::<Result<u32, !>>() == 4);
    check(core::mem::size_of::<Result<u32, Infallible>>() == 4);
    check(core::mem::size_of::<Option<!>>() == 0);
    check(core::mem::size_of::<(u32, !)>() == 4);
    check(irrefutable(Ok(3)) == 3);
    check(one_arm(Ok(4)) == 4);
    check(by_ref(&Ok(5)) == 5);
    let none: Option<!> = None;
    check(none.is_none());
    let x = if true { None } else { Some(loop {}) };
    check(size(&x) == 0);
    check(tag_of(&x) == 2);
}
"#,
    );
}

/// 1.100's `aligned_fields_in_packed` is a lint where E0588 was: the layout is the packed
/// one, the aligned field at its packed offset and at its own size.
#[test]
fn v1_100_an_aligned_field_in_a_packed_struct_is_laid_out_packed() {
    runs(
        "minicore: size_of, copy, offset_of",
        r#"
#[repr(C, align(16))] struct Al(i32);
#[allow(aligned_fields_in_packed)]
#[repr(C, packed)] struct P(u8, Al);
fn main() {
    check(core::mem::size_of::<Al>() == 16);
    check(core::mem::size_of::<P>() == 17);
    check(core::mem::offset_of!(P, 1) == 1);
}
"#,
    );
}

/// 1.100's compatibility note (rust-lang/rust#154210): a closure whose `if let` moves one
/// field captures that field, not the whole value, as `match` and `let` do. The other
/// field is still the owner's to drop, after the closure's.
#[test]
fn v1_100_if_let_in_a_closure_captures_only_the_moved_field() {
    runs(
        "minicore: drop, cell, add, mul, builtin_impls, copy, fn",
        r#"
LOG
struct Thing<'a>(D<'a>, D<'a>);
fn main() {
    let l = Cell::new(0);
    {
        let x = Thing(D(&l, 1), D(&l, 2));
        let closure = || { if let Thing(_a, _) = x {} };
        closure();
        log(&l, 3);
    }
    check(l.get() == 132);
}
"#,
    );
}

// Rust 1.99

/// rust-lang/rust#156047: a method called on a value of a diverging variable is `!`'s.
#[test]
fn v1_99_a_method_on_a_diverging_variable_is_nevers() {
    runs(
        "minicore: copy",
        r#"
trait Tr { fn m(&self) -> u8 { 1 } }
impl Tr for ! {}
fn f(c: bool) -> u8 { if c { return 7; } let x = loop { return 8; }; x.m() }
fn main() { check(f(true) == 7); check(f(false) == 8); }
"#,
    );
}

// Rust 1.96

/// rust-lang/rust#147834: `!` coerces in a tuple expression with no expected type.
#[test]
fn v1_96_never_coerces_in_a_tuple() {
    runs(
        "minicore: copy",
        r#"
fn f(c: bool) -> (u8, u8) { if c { return (1, 2); } let t: (u8, u8) = (loop { return (3, 4); }, 5); t }
fn g(c: bool) -> u8 { if c { return 1; } ((),) = (loop { return 2; },); 3 }
fn main() { check(f(true).0 == 1); check(f(false).1 == 4); check(g(false) == 2); }
"#,
    );
}

// Rust 1.95

/// `if let` guards (rust-lang/rust#141295): the guard's bindings are the arm's, a failed
/// guard falls to the next arm, and the temporaries of the guard's scrutinee are dropped
/// when the arm is left, after its body (the Reference, `destructors.scope.temporary`:
/// "pattern-matching guards' temporaries are dropped when leaving the arm scope").
#[test]
fn v1_95_if_let_guards() {
    runs(
        "minicore: option, add, mul, drop, cell, builtin_impls, copy, ord",
        r#"
LOG
impl<'a> D<'a> { fn some(&self) -> Option<u32> { Some(self.1) } }
fn f(x: Option<Option<u8>>) -> u8 {
    match x {
        Some(y) if let Some(z) = y && z > 1 => z,
        Some(y) if let Some(z) = y => z + 10,
        Some(_) if let None = None::<u8> => 100,
        _ => 9,
    }
}
fn main() {
    check(f(Some(Some(5))) == 5);
    check(f(Some(Some(1))) == 11);
    check(f(Some(None)) == 100);
    check(f(None) == 9);
    let l = Cell::new(0);
    match 0u8 {
        _ if let Some(n) = D(&l, 1).some() => { log(&l, 2); check(n == 1); }
        _ => check(false),
    }
    check(l.get() == 21);
}
"#,
    );
}

// Rust 1.93

/// rust-lang/rust#148602: the LUB of a safe and an unsafe function item is an unsafe
/// function pointer to whichever was taken. rust-lang/rust#148259: a pointer copied byte by
/// byte in const evaluation keeps what it points to.
#[test]
fn v1_93_lub_of_fn_items_and_bytewise_pointer_copies_in_const() {
    runs(
        "minicore: copy, add, ord, index, slice, builtin_impls",
        r#"
fn a() -> u8 { 1 } unsafe fn b() -> u8 { 2 }
fn pick(c: bool) -> unsafe fn() -> u8 { let f = if c { a } else { b }; f }
fn matched(c: u8) -> unsafe fn() -> u8 { match c { 0 => a, _ => b } }
union U { r: &'static u8, b: [u8; 8] }
const C: &u8 = {
    let src = U { r: &7 };
    let mut dst = U { b: [0; 8] };
    unsafe {
        let mut i = 0;
        while i < 8 { dst.b[i] = src.b[i]; i += 1; }
        dst.r
    }
};
fn main() {
    unsafe { check(pick(true)() == 1); check(pick(false)() == 2); check(matched(0)() == 1); check(matched(1)() == 2); }
    check(*C == 7);
}
"#,
    );
}

// Rust 1.92

/// rust-lang/rust#145277: `[CONST; 0]` makes no value of the constant, so drops none; and
/// `&raw` of a union's field in safe code is a pointer to the field.
#[test]
fn v1_92_zero_length_repeats_and_raw_borrows_of_union_fields() {
    runs(
        "minicore: drop, add, builtin_impls, copy",
        r#"
static mut N: u32 = 0;
struct Dr; impl Drop for Dr { fn drop(&mut self) { unsafe { N += 1; } } }
const C: Dr = Dr;
union Un { a: u8, b: u32 }
fn main() {
    { let _x: [Dr; 0] = [C; 0]; }
    unsafe { check(N == 0); }
    { let _y: [Dr; 2] = [C; 2]; }
    unsafe { check(N == 2); }
    let mut u = Un { b: 0 };
    let q = &raw mut u.a;
    unsafe { *q = 3; check(u.a == 3); }
}
"#,
    );
}

// Rust 1.91

/// rust-lang/rust#143764: the bindings of an or-pattern are dropped in the reverse of the
/// order the first alternative writes them, whichever alternative matched.
#[test]
fn v1_91_bindings_drop_in_the_order_of_the_first_alternative() {
    runs(
        "minicore: drop, cell, add, mul, builtin_impls, copy",
        r#"
LOG
fn main() {
    let l = Cell::new(0);
    {
        match (D(&l, 1), D(&l, 2), false) { (x, y, true) | (y, x, false) => { let _ = (&x, &y); } }
    }
    // `y` (holding 1) is declared after `x` (holding 2), so it is dropped first.
    check(l.get() == 12);
    let l = Cell::new(0);
    { let (a, b) = (D(&l, 1), D(&l, 2)); let _ = (&a, &b); }
    check(l.get() == 21);
}
"#,
    );
}

// Rust 1.89

/// `repr128` (the discriminant is 128 bits wide), `generic_arg_infer` (`_` is the length
/// the use decides), and rust-lang/rust#140593 (a temporary borrowed in the argument of a
/// tuple struct's or a variant's constructor in a `let` lives to the end of the block).
#[test]
fn v1_89_repr128_inferred_lengths_and_extended_temporaries() {
    runs(
        "minicore: drop, cell, add, mul, builtin_impls, copy, option, slice",
        r#"
LOG
#[repr(u128)] enum E { A = 1 << 100, B }
#[repr(i128)] enum F { A = -1, B }
struct A<const N: usize>([u8; N]);
impl<const N: usize> A<N> { fn n(&self) -> usize { N } }
struct W<'a, 'b>(&'a D<'b>);
fn main() {
    check(E::A as u128 == 1u128 << 100);
    check(E::B as u128 == (1u128 << 100) + 1);
    check(F::A as i128 == -1);
    check(F::B as i128 == 0);

    let c = [1u8; _];
    let d: [u8; 4] = c;
    let a: A<_> = A([0; 2]);
    let b: [u8; _] = [0; 3];
    check(a.n() == 2); check(b.len() == 3); check(d.len() == 4);

    let l = Cell::new(0);
    {
        let w = W(&D(&l, 1));
        let s = Some(&D(&l, 2));
        log(&l, 3);
        let _ = (w.0.1, s);
    }
    check(l.get() == 321);
}
"#,
    );
}

// Rust 1.88

/// `let` chains in the 2024 edition: each `let` and condition in turn, a chain's bindings
/// in the block, and its temporaries dropped at the end of the `if`, last made first.
#[test]
fn v1_88_let_chains() {
    runs(
        "minicore: drop, cell, add, mul, builtin_impls, copy, option, ord\n//- /main.rs edition:2024",
        r#"
LOG
impl<'a> D<'a> { fn some(&self) -> Option<u32> { Some(self.1) } }
fn f(a: Option<u8>, b: Option<u8>) -> u8 { if let Some(x) = a && x > 1 && let Some(y) = b { x + y } else { 0 } }
fn main() {
    check(f(Some(2), Some(3)) == 5); check(f(Some(1), Some(3)) == 0); check(f(Some(2), None) == 0);
    let mut it = Some(3u8); let mut n = 0u8;
    while let Some(x) = it && x > 0 { n = n + 1; it = if x > 1 { Some(x - 1) } else { None }; }
    check(n == 3);

    let l = Cell::new(0);
    if let Some(a) = D(&l, 1).some() && let Some(b) = D(&l, 2).some() { log(&l, 3); let _ = (a, b); }
    log(&l, 4);
    check(l.get() == 3214);
}
"#,
    );
}

// Rust 1.86 and 1.87

/// `trait_upcasting`: a `&dyn Sub` coerced to each supertrait calls the supertrait's method
/// of the same value. rust-lang/rust#135480: an impl for `dyn Other` leaves out the
/// `Self: Sized` method and its other method is called through the object.
#[test]
fn v1_86_and_1_87_upcasting_and_sized_methods_left_out() {
    runs(
        "minicore: coerce_unsized, dispatch_from_dyn, add, builtin_impls, copy",
        r#"
trait A { fn a(&self) -> u8; }
trait A2 { fn a2(&self) -> u8; }
trait B: A + A2 { fn b(&self) -> u8; }
struct S(u8);
impl A for S { fn a(&self) -> u8 { self.0 } }
impl A2 for S { fn a2(&self) -> u8 { self.0 + 1 } }
impl B for S { fn b(&self) -> u8 { self.0 + 2 } }

trait Tr { fn sized(self) -> u8 where Self: Sized; fn a(&self) -> u8; }
trait Other { fn o(&self) -> u8; }
impl Tr for dyn Other { fn a(&self) -> u8 { self.o() } }
impl Other for S { fn o(&self) -> u8 { 4 } }
fn main() {
    let s = S(5);
    let b: &dyn B = &s;
    let a: &dyn A = b;
    let a2: &dyn A2 = b;
    check(a.a() == 5); check(a2.a2() == 6); check(b.b() == 7);
    let d: &dyn Other = &s;
    check(Tr::a(d) == 4); check(d.a() == 4);
}
"#,
    );
}

// Rust 1.85: the 2024 edition

const IF_LET_RESCOPE: &str = r#"
LOG
fn mk<'a>(l: &'a Cell<u32>) -> D<'a> { D(l, 1) }
impl<'a> D<'a> { fn none(&self) -> Option<u8> { None } fn val(&self) -> u32 { self.1 } }
fn tail<'a>(l: &'a Cell<u32>) -> u32 { let _local = D(l, 2); D(l, 1).val() }
fn main() {
    // The scrutinee's temporary (1) and the `else` block (2).
    let l = Cell::new(0);
    if let Some(_) = mk(&l).none() { } else { log(&l, 2); }
    check(l.get() == IF_LET);
    // The tail expression's temporary (1) and the block's local (2).
    let l = Cell::new(0);
    check(tail(&l) == 1);
    check(l.get() == TAIL);
}
"#;

/// The 2024 edition drops the temporaries of an `if let` scrutinee before its `else` block,
/// and those of a block's tail expression before the block's locals (`if_let_rescope`,
/// `shorter_tail_lifetimes`); every edition before it, after. The same program, both ways.
#[test]
fn v1_85_edition_2024_temporary_scopes() {
    let flags = "minicore: drop, cell, add, mul, builtin_impls, copy, option";
    let source =
        |if_let: &str, tail: &str| IF_LET_RESCOPE.replace("IF_LET", if_let).replace("TAIL", tail);
    runs(&format!("{flags}\n//- /main.rs edition:2024"), &source("12", "12"));
    runs(&format!("{flags}\n//- /main.rs edition:2021"), &source("21", "21"));
}

// Rust 1.84

/// rust-lang/rust#131857: dropping the principal of a trait object keeps the value, whose
/// destructor still runs once.
#[test]
fn v1_84_dropping_the_principal_keeps_the_value() {
    runs(
        "minicore: coerce_unsized, dispatch_from_dyn, send, drop, cell, add, mul, builtin_impls, copy",
        r#"
LOG
trait Tr {} impl<'a> Tr for D<'a> {}
fn main() {
    let l = Cell::new(0);
    { let d = D(&l, 7); let r: &(dyn Tr + Send) = &d; let _s: &dyn Send = r; }
    check(l.get() == 7);
}
"#,
    );
}

// Rust 1.80 to 1.83

/// 1.83 `const_mut_refs` and `const_refs_to_static`, 1.82 floats in `const fn`, nested
/// `offset_of!`, `&raw` of a `static mut` and of a dereferenced pointer, and 1.80's
/// exclusive range patterns.
#[test]
fn v1_80_to_1_83_const_evaluation_raw_borrows_and_range_patterns() {
    runs(
        "minicore: cell, copy, add, mul, builtin_impls, offset_of",
        r#"
const fn inc(x: &mut i32) { *x += 1; }
const A: i32 = { let mut a = 1; inc(&mut a); let p = &raw mut a; unsafe { *p += 1 }; a };
static S: i32 = 5;
const R: &i32 = &S;

const fn f(x: f64, y: f32) -> f64 { x * 2.0 + (y as f64) / 4.0 - 1.0 }
const X: f64 = f(1.5, 2.0);
const B: bool = 1.0f32 < 2.0f32;

#[repr(C)] struct Outer { x: u8, b: Mid } #[repr(C)] struct Mid { y: u16, c: Inner } #[repr(C)] struct Inner(u8, u32);
const O: usize = core::mem::offset_of!(Outer, b.c.1);

static mut M: i32 = 1;
fn range(x: u8) -> u8 { match x { 0..10 => 0, 10..=20 => 1, 21.. => 2 } }
fn main() {
    check(A == 3); check(*R == 5);
    check(X == 2.5); check(B);
    check(O == 12); check(core::mem::offset_of!(Outer, b.c) == 8);
    let p = &raw mut M; unsafe { *p += 1; check(*(&raw const M) == 2); }
    let mut x = 5u8; let r = &raw mut x; let r2 = &raw mut *r; unsafe { *r2 = 6; } check(x == 6);
    check(range(0) == 0); check(range(9) == 0); check(range(10) == 1); check(range(20) == 1);
    check(range(21) == 2); check(range(255) == 2);
}
"#,
    );
}

/// 1.81 (rust-lang/rust#116088): a panic that would unwind out of an `extern "C"` function
/// aborts the program there; out of an `extern "C-unwind"` one it unwinds, a panic.
#[test]
fn v1_81_a_panic_out_of_an_extern_c_function_aborts() {
    let aborts = |abi: &str| {
        let source = format!("extern \"{abi}\" fn f() {{ panic!(\"x\") }}\nfn main() {{ f() }}\n");
        let mut aborted = false;
        check_error_with(&fixture("minicore: copy", &source), |e| {
            aborted = format!("{e:?}").contains("cannot unwind");
            true
        });
        aborted
    };
    assert!(aborts("C"), "a panic in `extern \"C\"` aborts");
    assert!(!aborts("C-unwind"), "a panic in `extern \"C-unwind\"` unwinds");
}

/// 1.80 (rust-lang/rust#117329): a read and a write of a zero-sized type through a pointer
/// to no allocation are no undefined behavior.
#[test]
fn v1_80_zero_sized_accesses_through_any_pointer() {
    runs(
        "minicore: copy",
        r#"
fn main() { unsafe {
    let p = 16usize as *const ();
    let _: () = *p;
    let q = 16usize as *mut [u8; 0];
    *q = [];
} }
"#,
    );
}
