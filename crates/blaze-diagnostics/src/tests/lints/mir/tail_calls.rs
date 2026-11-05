//! rustc's `check_tail_calls` (`compiler/rustc_mir_build/src/check_tail_calls.rs`), as
//! hir-ty-tailck ports it: what it refuses of a `become`, the `tail_call_track_caller` lint,
//! and the borrow check of a tail call's arguments, from rustc's UI tests
//! (`tests/ui/explicit-tail-calls/`).

use crate::tests::{checked, items::ui_core, ui};

/// The errors of a crate of `main.rs` and `minicore` (`flags`), each as `4:17 message`: the
/// line and column of its primary span in `source`, and its message.
fn errors(flags: &str, source: &str) -> Vec<String> {
    let checked =
        checked(&format!("//- minicore: {flags}\n//- /main.rs crate:other:main\n{source}"));
    checked
        .diags
        .iter()
        .filter(|it| it.is_error() && !it.is_lint())
        .filter_map(|diag| {
            let span = diag.span.primary.first()?;
            let before = &source[..span.lo as usize];
            let line = before.matches('\n').count() + 1;
            let column = before[before.rfind('\n').map_or(0, |it| it + 1)..].chars().count() + 1;
            Some(format!("{line}:{column} {}", diag.message))
        })
        .collect()
}

#[test]
fn become_needs_matching_signatures_and_abis() {
    // tests/ui/explicit-tail-calls/signature-mismatch.rs
    let source = r#"#![expect(incomplete_features)]
#![feature(explicit_tail_calls, rust_tail_cc)]

fn _f0((): ()) {
    become _g0(); //~ error: mismatched signatures
}

fn _g0() {}

fn _f1() {
    become _g1(()); //~ error: mismatched signatures
}

fn _g1((): ()) {}

extern "C" fn _f2() {
    become _g2(); //~ error: mismatched function ABIs
}

fn _g2() {}

fn _f3() {
    become _g3(); //~ error: mismatched function ABIs
}

extern "C" fn _g3() {}

extern "tail" fn _tailcc() {}

fn _f4() {
    // tailcc does not need the signatures to match,
    // but only tailcc can tail call tailcc.
    become _tailcc(); //~ error: mismatched function ABIs
}

fn main() {}
"#;
    // tests/ui/explicit-tail-calls/signature-mismatch.stderr
    let stderr = r#"error: mismatched signatures
  --> $DIR/signature-mismatch.rs:5:5
   |
LL |     become _g0();
   |     ^^^^^^^^^^^^
   |
   = note: `become` requires caller and callee to have matching signatures
   = note: caller signature: `fn(())`
   = note: callee signature: `fn()`

error: mismatched signatures
  --> $DIR/signature-mismatch.rs:11:5
   |
LL |     become _g1(());
   |     ^^^^^^^^^^^^^^
   |
   = note: `become` requires caller and callee to have matching signatures
   = note: caller signature: `fn()`
   = note: callee signature: `fn(())`

error: mismatched function ABIs
  --> $DIR/signature-mismatch.rs:17:5
   |
LL |     become _g2();
   |     ^^^^^^^^^^^^
   |
   = note: `become` requires caller and callee to have the same ABI
   = note: caller ABI is `"C"`, while callee ABI is `"Rust"`

error: mismatched function ABIs
  --> $DIR/signature-mismatch.rs:23:5
   |
LL |     become _g3();
   |     ^^^^^^^^^^^^
   |
   = note: `become` requires caller and callee to have the same ABI
   = note: caller ABI is `"Rust"`, while callee ABI is `"C"`

error: mismatched function ABIs
  --> $DIR/signature-mismatch.rs:33:5
   |
LL |     become _tailcc();
   |     ^^^^^^^^^^^^^^^^
   |
   = note: `become` requires caller and callee to have the same ABI
   = note: caller ABI is `"Rust"`, while callee ABI is `"tail"`

error: aborting due to 5 previous errors

"#;
    assert_eq!(ui("signature-mismatch.rs", "2015", source), stderr);
}

#[test]
fn tail_call_arguments_must_outlive_the_caller_frame() {
    // tests/ui/explicit-tail-calls/ctfe-arg-bad-borrow.rs: the caller's locals die before the
    // callee runs, so a borrow of one passed to it is E0597.
    let source = r#"#![expect(incomplete_features)]
#![feature(explicit_tail_calls)]

pub const fn test(_: &Type) {
    const fn takes_borrow(_: &Type) {}

    let local = Type;
    become takes_borrow(&local);
    //~^ error: `local` does not live long enough
}

struct Type;

fn main() {}
"#;
    // tests/ui/explicit-tail-calls/ctfe-arg-bad-borrow.stderr
    let stderr = r#"error[E0597]: `local` does not live long enough
  --> $DIR/ctfe-arg-bad-borrow.rs:8:25
   |
LL |     let local = Type;
   |         ----- binding `local` declared here
LL |     become takes_borrow(&local);
   |                         ^^^^^^- `local` dropped here while still borrowed
   |                         |
   |                         borrowed value does not live long enough

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0597`.
"#;
    assert_eq!(ui("ctfe-arg-bad-borrow.rs", "2015", source), stderr);
}

#[test]
fn tail_calling_a_track_caller_fn_warns() {
    // tests/ui/explicit-tail-calls/callee_is_track_caller.rs, its `main` without `assert_eq!`
    // (rustc gives the lint from codegen; blaze from the front end)
    let source = r#"#![expect(incomplete_features)]
#![feature(explicit_tail_calls)]

fn a(x: u32) -> u32 {
    become b(x);
    //~^ warning: tail calling a function marked with `#[track_caller]` has no special effect
}

#[track_caller]
fn b(x: u32) -> u32 { x + 42 }

fn main() {
    a(12);
}
"#;
    let stderr = r#"warning: tail calling a function marked with `#[track_caller]` has no special effect
  --> $DIR/callee_is_track_caller.rs:5:12
   |
LL |     become b(x);
   |            ^^^^
   |
   = note: `#[warn(tail_call_track_caller)]` on by default

warning: 1 warning emitted

"#;
    assert_eq!(ui_core("callee_is_track_caller.rs", "2015", "add", source), stderr);
}

#[test]
fn tail_calling_a_trait_fn_declared_track_caller_warns() {
    // tests/ui/explicit-tail-calls/callee_is_track_caller_polymorphic.rs: `T::f` is
    // `#[track_caller]` by its declaration in the trait, whatever `T` is
    let source = r#"#![expect(incomplete_features)]
#![feature(explicit_tail_calls)]

fn c<T: Trait>() {
    become T::f();
    //~^ warning: tail calling a function marked with `#[track_caller]` has no special effect
}

trait Trait {
    #[track_caller]
    fn f() {}
}

impl Trait for () {}

fn main() {
    c::<()>();
}
"#;
    let stderr = r#"warning: tail calling a function marked with `#[track_caller]` has no special effect
  --> $DIR/callee_is_track_caller_polymorphic.rs:5:12
   |
LL |     become T::f();
   |            ^^^^^^
   |
   = note: `#[warn(tail_call_track_caller)]` on by default

warning: 1 warning emitted

"#;
    assert_eq!(ui("callee_is_track_caller_polymorphic.rs", "2015", source), stderr);
}

#[test]
fn signatures_compare_with_regions_erased() {
    // tests/ui/explicit-tail-calls/return-lifetime-sub.rs, higher-ranked-arg.rs and
    // become-trait-fn.rs: `check-pass`
    let source = r#"#![allow(incomplete_features)]
#![feature(explicit_tail_calls)]

fn _f<'a>() -> &'a [u8] {
    become _g();
}

fn _g() -> &'static [u8] {
    &[0, 1, 2, 3]
}

fn foo(x: fn(&i32)) {
    become bar(x);
}

fn bar(_: fn(&i32)) {}

trait Tr {
    fn f();
}

fn g<T: Tr>() {
    become T::f();
}

fn main() {}
"#;
    assert_eq!(errors("sized, unsize, coerce_unsized, slice", source), [] as [&str; 0]);
}

#[test]
fn what_become_refuses() {
    // tests/ui/explicit-tail-calls/{in-closure, become-uncallable, callee_is_ref, closure,
    // intrinsics, caller_is_track_caller, become-operator}.rs, in one crate on minicore (no
    // `Box`, `String` or `Wrapping`: a type of the test's own stands in for them)
    let source = r#"#![allow(incomplete_features)]
#![feature(explicit_tail_calls)]

fn in_closure() {
    let _ = || become f();
}

fn f() {}

fn uncallable() -> u64 {
    become 1;
}

fn block() {
    become { f() };
}

fn deref() {
    become *&f();
}

fn by_ref() {
    become (&f)()
}

fn by_ref_ptr() {
    let fun: &fn() = &(f as fn());
    become fun();
}

fn closure() {
    become (|| ())();
}

fn closure_args((): (), _: i32) {
    become (|(), _| ())((), 1);
}

fn intrinsic((): ()) {
    unsafe { become core::mem::transmute(()) }
}

#[track_caller]
fn tracked() {
    become f();
}

trait Trait {
    fn d(&self);

    #[track_caller]
    fn e(&self) {
        become self.d();
    }
}

fn builtin_add(a: u64, b: u64) -> u64 {
    become a + b;
}

fn builtin_not(x: u64) -> u64 {
    become !x;
}

fn builtin_index(x: &[u8], i: usize) -> u8 {
    become x[i]
}

fn builtin_assign(a: &mut u8, _: u8) {
    become *a += 1;
}

struct W(u8);

impl core::ops::Add for W {
    type Output = W;
    fn add(self, _: W) -> W { self }
}

impl core::ops::Not for W {
    type Output = W;
    fn not(self) -> W { self }
}

impl core::ops::AddAssign<u8> for W {
    fn add_assign(&mut self, _: u8) {}
}

fn overloaded_add(a: W, b: W) -> W {
    become a + b;
}

fn overloaded_not(x: W) -> W {
    become !x;
}

fn overloaded_assign(b: &mut W, _: u8) {
    become *b += 1;
}

fn main() {}
"#;
    assert_eq!(
        errors("add, unary_ops, builtin_impls, index, slice, fn, transmute", source),
        [
            "5:16 `become` is not allowed in closures",
            "11:12 `become` requires a function call",
            "15:12 `become` requires a function call",
            "19:12 `become` requires a function call",
            "23:12 tail calls can only be performed with function definitions or pointers",
            "28:12 tail calls can only be performed with function definitions or pointers",
            "32:5 tail calling closures directly is not allowed",
            "36:5 tail calling closures directly is not allowed",
            "40:14 tail calling intrinsics is not allowed",
            "45:5 a function marked with `#[track_caller]` cannot perform a tail-call",
            "53:9 a function marked with `#[track_caller]` cannot perform a tail-call",
            "58:12 `become` does not support operators",
            "62:12 `become` does not support operators",
            "66:12 `become` does not support operators",
            "70:12 `become` does not support operators",
            "90:12 `become` does not support operators",
            "94:12 `become` does not support operators",
            "98:12 `become` does not support operators",
        ]
    );
}
