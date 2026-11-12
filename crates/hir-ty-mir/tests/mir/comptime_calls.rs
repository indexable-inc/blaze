//! `const_check::comptime::comptime_calls`: a compile-time-only function (`#[rustc_comptime]`)
//! called where the call would run, which the pinned nightly refuses with "comptime functions
//! can only be called at compile time" (asked of its rustc with these bodies, 2026-11-12).

use hir_def::{DefWithBodyId, expr_store::Body};
use hir_expand::fixture;
use hir_ty_mir::mir::{MirSpan, const_check::comptime::comptime_calls};

use crate::function;

const FACTS: &str = r#"
//- minicore: sized, fn
#[rustc_comptime]
pub fn fact(x: usize) -> usize { x }
#[rustc_intrinsic]
#[rustc_comptime]
pub fn raw<T>() -> usize;
"#;

/// The text of each call `comptime_calls` reports in the function `name` of `FACTS` and `src`.
#[track_caller]
fn refused(src: &str, name: &str) -> Vec<String> {
    let src = format!("{FACTS}{src}");
    let fx = fixture::db(&src);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let def = DefWithBodyId::FunctionId(function(&db, name));
        let map = Body::source_map(&db, def);
        comptime_calls(&db, def)
            .calls
            .iter()
            .map(|call| {
                let MirSpan::ExprId(expr) = call.span else { panic!("{:?}", call.span) };
                let range = map.expr_syntax(expr).expect("written syntax").value.text_range();
                src[usize::from(range.start())..usize::from(range.end())].to_owned()
            })
            .collect()
    })
}

/// The cause of bstd's build failure: a `const fn` may run, so its call is refused as a
/// function's is.
#[test]
fn a_call_that_would_run_is_refused() {
    assert_eq!(refused("pub fn runs() -> usize { fact(1) }", "runs"), ["fact(1)"]);
    assert_eq!(refused("pub const fn may_run() -> usize { fact(2) }", "may_run"), ["fact(2)"]);
    assert_eq!(refused("pub fn asks<T>() -> usize { raw::<T>() }", "asks"), ["raw::<T>()"]);
}

/// The same calls with the cause removed: in a `const { .. }` block, which is a constant of
/// its own, and in a function that is itself compile-time-only.
#[test]
fn a_call_at_compile_time_is_not() {
    let none: [&str; 0] = [];
    assert_eq!(refused("pub fn runs() -> usize { const { fact(1) } }", "runs"), none);
    assert_eq!(refused("pub const fn may_run<T>() -> usize { const { raw::<T>() } }", "may_run"), none);
    assert_eq!(refused("#[rustc_comptime] pub fn only() -> usize { fact(3) + raw::<u8>() }", "only"), none);
    assert_eq!(refused("pub const N: usize = fact(4);\npub fn reads() -> usize { N }", "reads"), none);
}

/// A closure of a function runs when the function calls it.
#[test]
fn a_call_in_a_closure_is_refused() {
    assert_eq!(
        refused("pub fn runs() -> usize { let f = || fact(5); f() }", "runs"),
        ["fact(5)"]
    );
}
