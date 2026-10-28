use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: const-check
//
// An error of rustc's const checker: a `const fn`, a `const` or a `static` calls a function
// that is not `const` or a function pointer, casts a pointer to an integer, compares pointers,
// or has inline assembly.
pub(crate) fn const_check_error(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::ConstCheckError,
) -> Diagnostic {
    let code = match d.code {
        "" => DiagnosticCode::Ra("const-check", crate::Severity::Error),
        code => DiagnosticCode::RustcHardError(code),
    };
    Diagnostic::new_with_syntax_node_ptr(ctx, code, d.message.clone(), d.node).stable()
}

// Diagnostic: const-impure
//
// A `const fn` whose result at run time depends on more than its arguments: it reads or
// writes a static that changes (a `static mut`, one that is not `Freeze`, a thread-local or a
// foreign static), or a `const fn` it calls does. `#[blaze::cache]` can't keep such a result.
pub(crate) fn const_impure(ctx: &DiagnosticsContext<'_, '_>, d: &hir::ConstImpure) -> Diagnostic {
    let code = DiagnosticCode::Ra("const-impure", crate::Severity::Warning);
    Diagnostic::new_with_syntax_node_ptr(ctx, code, d.message.clone(), d.node).stable()
}

#[cfg(test)]
mod tests {
    use crate::tests::check_diagnostics;

    #[test]
    fn non_const_calls_in_const_contexts() {
        check_diagnostics(
            r#"
fn regular() -> u32 { 1 }
const fn constant() -> u32 { 2 }
mod m {
    pub fn inner() -> u32 { 3 }
}
struct S;
impl S {
    fn new() -> S { S }
    fn get(&self) -> u32 { 4 }
    const fn made() -> S { S }
}
const fn f() -> u32 {
    let s = S::made();
    constant();
    regular();
  //^^^^^^^^^ error: cannot call non-const function `regular` in constant functions
    m::inner();
  //^^^^^^^^^^ error: cannot call non-const function `m::inner` in constant functions
    s.get()
  //^^^^^^^ error: cannot call non-const method `S::get` in constant functions
}
const C: u32 = regular();
             //^^^^^^^^^ error: cannot call non-const function `regular` in constants
static X: S = S::new();
            //^^^^^^^^ error: cannot call non-const associated function `S::new` in statics
fn runtime() -> u32 { regular() }
"#,
        );
    }

    #[test]
    fn trait_methods_and_unreachable_calls_are_left_to_rustc() {
        check_diagnostics(
            r#"
//- minicore: default, panic
trait T { fn t() -> u32; }
impl T for () { fn t() -> u32 { 0 } }
fn regular() -> u32 { 1 }
const fn generic<X: T>() -> u32 { X::t() }
const fn dead() -> u32 {
    return 0;
    regular()
}
const fn panics() -> u32 {
    panic!("no")
}
struct Tuple(u32);
const fn built() -> Tuple { Tuple(1) }
"#,
        );
    }

    #[test]
    fn atomics_in_const_contexts() {
        check_diagnostics(
            r#"
// The nightly's atomic intrinsics are `const` (`#![feature(const_atomic)]`), as `atomic_load` is
// here; one not declared `const` is refused, as rustc refused them before.
#[rustc_intrinsic]
const unsafe fn atomic_load<T: Copy, const ORD: u8>(src: *const T) -> T;
#[rustc_intrinsic]
unsafe fn atomic_xadd<T: Copy, const ORD: u8>(dst: *mut T, src: T) -> T;
const unsafe fn load(p: *const u32) -> u32 {
    unsafe { atomic_load::<u32, 0>(p) }
}
const unsafe fn add(p: *mut u32) -> u32 {
    unsafe { atomic_xadd::<u32, 0>(p, 1) }
           //^^^^^^^^^^^^^^^^^^^^^^^^^^^ error: cannot call non-const intrinsic `atomic_xadd` in constant functions
}
// User code reaches atomics in const only through calls rustc refuses (E0015).
fn bump(p: *mut u32) -> u32 {
    unsafe { atomic_xadd::<u32, 0>(p, 1) }
}
struct Counter;
impl Counter {
    fn fetch_add(&self) -> u32 { 0 }
}
const fn via_fn(p: *mut u32) -> u32 {
    bump(p)
  //^^^^^^^ error: cannot call non-const function `bump` in constant functions
}
const C: u32 = Counter.fetch_add();
             //^^^^^^^^^^^^^^^^^^^ error: cannot call non-const method `Counter::fetch_add` in constants
"#,
        );
    }

    #[test]
    fn comptime_intrinsics_in_constants() {
        // core writes `size_of`, `align_of` and `needs_drop` as `#[rustc_comptime] pub fn`,
        // which rustc calls always const.
        check_diagnostics(
            r#"
//- minicore: size_of
const S: usize = core::mem::size_of::<u64>();
static T: usize = core::mem::size_of::<u32>();
"#,
        );
    }

    #[test]
    fn pointers_in_const_fns() {
        check_diagnostics(
            r#"
//- minicore: eq, fn
const fn address(p: *const u8) -> usize {
    p as usize
  //^^^^^^^^^^ error: pointers cannot be cast to integers during const eval
}
const fn same(p: *const u8, q: *const u8) -> bool {
    p == q
  //^^^^^^ error: pointers cannot be reliably compared during const eval
}
const fn call(f: fn() -> u32) -> u32 {
    f()
  //^^^ error: function pointer calls are not allowed in constant functions
}
fn runtime(p: *const u8, f: fn() -> u32) -> usize {
    f();
    p as usize
}
"#,
        );
    }

    #[test]
    fn const_fns_that_read_mutable_state() {
        check_diagnostics(
            r#"
//- minicore: cell, sync
use core::cell::UnsafeCell;
static mut COUNT: u32 = 0;
struct Counter(UnsafeCell<u32>);
unsafe impl Sync for Counter {}
static CALLS: Counter = Counter(UnsafeCell::new(0));
static PLAIN: u32 = 1;
const fn count() -> u32 {
    unsafe { COUNT }
           //^^^^^ warn: const fn reads or writes mutable state: `COUNT` is a `static mut`
}
const fn indirect() -> u32 {
    count()
  //^^^^^^^ warn: const fn reaches `count`, which reads or writes mutable state: `COUNT` is a `static mut`
}
const fn calls() -> &'static Counter {
    &CALLS
   //^^^^^ warn: const fn reads or writes mutable state: `CALLS` is `Counter`, which is not `Freeze`
}
const fn plain() -> u32 { PLAIN }
fn runtime() -> u32 { unsafe { COUNT } }
"#,
        );
    }
}
