//! `const_check`, against what rustc's `check_consts` reports at the pinned nightly: each test
//! ports the rustc ui test it names (`tests/ui/...`), with its expected errors. Where the ui
//! test uses std's const traits, minicore's traits are not const, and the kind is the
//! non-const one of the same call; the message and span of a desugaring follow std's types,
//! which minicore's don't spell the same, so those tests check the kind and code.

use hir_def::{
    AssocItemId, DefWithBodyId,
    expr_store::{Body, ExpressionStore},
};
use hir_ty::InferenceResult;
use hir_ty_mir::mir::{
    MirSpan,
    const_check::{
        ClosureNote, ConstCheckDiagnostic, ConstCheckKind, Desugaring, anon_const_check, const_check,
    },
};
use hir_expand::fixture;

use crate::{constant, db::TestDB, function};

/// What `const_check` reports in the body `pick` finds in `src` with `minicore`'s flags: each
/// diagnostic with the text its span covers, `<macro>` for a macro's.
#[track_caller]
fn check(
    minicore: &str,
    src: &str,
    pick: impl FnOnce(&TestDB) -> DefWithBodyId,
) -> Vec<(ConstCheckDiagnostic, String)> {
    let fx = fixture::db(&format!("//- minicore: {minicore}\n{src}"));
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let def = pick(&db);
        let (_, map) = Body::with_source_map(&db, def);
        let at = |span: MirSpan| {
            let source = match span {
                MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => map.expr_syntax(expr),
                MirSpan::PatId(pat) => map.pat_syntax(pat),
                other => panic!("a span with no syntax: {other:?}"),
            }
            .unwrap_or_else(|_| panic!("synthetic syntax at {span:?}"));
            if source.file_id.is_macro() {
                return "<macro>".to_owned();
            }
            let range = source.value.text_range();
            src[usize::from(range.start())..usize::from(range.end())].to_owned()
        };
        const_check(&db, def).diagnostics.iter().map(|it| (it.clone(), at(it.span))).collect()
    })
}

/// What `anon_const_check` reports in each anonymous constant of the function `name`'s body,
/// with the text its span covers.
#[track_caller]
fn check_anon_consts(minicore: &str, src: &str, name: &str) -> Vec<(ConstCheckDiagnostic, String)> {
    let fx = fixture::db(&format!("//- minicore: {minicore}\n{src}"));
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let func = function(&db, name);
        let mut found = Vec::new();
        for &konst in InferenceResult::of(&db, func).defined_anon_consts() {
            let map = ExpressionStore::with_source_map(&db, konst.loc(&db).owner).1;
            for it in &anon_const_check(&db, konst).diagnostics {
                let MirSpan::ExprId(expr) = it.span else { panic!("a span of no expression: {:?}", it.span) };
                let range = map.expr_syntax(expr).expect("written syntax").value.text_range();
                let text = src[usize::from(range.start())..usize::from(range.end())].to_owned();
                found.push((it.clone(), text));
            }
        }
        found
    })
}

/// `[0; f(2)]` in a function that is no const context: the array length is a constant of its
/// own, which rustc const checks, and a call of a function that is not `const` is refused there.
#[test]
fn a_non_const_call_in_an_array_length_is_refused() {
    let src = "fn f(x: usize) -> usize { x }\nfn main() {\n    let _ = [0u8; f(2)];\n}\n";
    let found = check_anon_consts("sized", src, "main");
    let [(refused, at)] = &found[..] else { panic!("{} diagnostics", found.len()) };
    assert_eq!(at, "f(2)");
    assert_eq!(refused.code, "E0015");
    assert!(matches!(refused.kind, ConstCheckKind::NonConstFn { conditionally: false, .. }));
    assert!(refused.message.contains("`f`") && refused.message.contains("constants"), "{}", refused.message);
    let callable = src.replace("fn f", "const fn f");
    assert_eq!(reported(&check_anon_consts("sized", &callable, "main")), NONE);
}

/// The code, message and source text of each diagnostic.
fn reported(found: &[(ConstCheckDiagnostic, String)]) -> Vec<(&'static str, &str, &str)> {
    found.iter().map(|(it, text)| (it.code, it.message.as_str(), text.as_str())).collect()
}

fn func(name: &'static str) -> impl FnOnce(&TestDB) -> DefWithBodyId {
    move |db| function(db, name).into()
}

/// The method `name` of the one `impl` of the fixture.
fn impl_method(name: &'static str) -> impl FnOnce(&TestDB) -> DefWithBodyId {
    move |db| {
        let impls = hir_def::test_items::impls(db, fixture::test_crate(&db));
        let [impl_] = impls[..] else { panic!("{} impls in the fixture", impls.len()) };
        let found = impl_.impl_items(db).items.iter().find_map(|(it, item)| match item {
            AssocItemId::FunctionId(func) if it.as_str() == name => Some(*func),
            _ => None,
        });
        found.unwrap_or_else(|| panic!("no method `{name}` in the impl")).into()
    }
}

const NONE: [(&str, &str, &str); 0] = [];

/// `tests/ui/consts/issue-56164.rs`.
const ISSUE_56164: &str = "\
const fn foo() { (||{})() }

const fn bad(input: fn()) {
    input()
}
";

#[test]
fn a_closure_call_is_a_non_const_closure() {
    let found = check("fn", ISSUE_56164, func("foo"));
    assert_eq!(
        reported(&found),
        [("E0015", "cannot call non-const closure in constant functions", "(||{})()")]
    );
    assert!(matches!(
        found[0].0.kind,
        ConstCheckKind::NonConstClosure { note: ClosureNote::Closure, conditionally: false, .. }
    ));
}

#[test]
fn a_function_pointer_call_is_refused() {
    let found = check("fn", ISSUE_56164, func("bad"));
    assert_eq!(
        reported(&found),
        [("", "function pointer calls are not allowed in constant functions", "input()")]
    );
    assert_eq!(found[0].0.kind, ConstCheckKind::FnPointerCall);
}

/// An `async` block in a `const fn` is refused at the block, naming it and the feature that
/// allows it, and allowed once `const_async_blocks` is on.
#[test]
fn an_async_block_needs_const_async_blocks() {
    let src = "const fn foo() -> impl core::future::Future<Output = ()> {\n    async {}\n}\n";
    let found = check("future", src, func("foo"));
    let [(refused, at)] = &found[..] else { panic!("{} diagnostics", found.len()) };
    assert_eq!(at, "async {}");
    assert_eq!(refused.kind, ConstCheckKind::Coroutine { what: "async blocks", gated: true });
    assert!(refused.message.contains("async blocks"), "{}", refused.message);
    let gated = format!("#![feature(const_async_blocks)]\n{src}");
    assert_eq!(reported(&check("future", &gated, func("foo"))), []);
}

/// A string pattern compares with `<str as PartialEq>::eq`, a call a `const fn` can't make
/// while `PartialEq` is not const (minicore's isn't): refused at the pattern, naming the type.
#[test]
fn a_string_pattern_is_a_match_comparison() {
    let src = "const fn foo(input: &'static str) {\n    match input {\n        \"a\" => (),\n        _ => (),\n    }\n}\n";
    let found = check("eq, str", src, func("foo"));
    let [(refused, at)] = &found[..] else { panic!("{} diagnostics", found.len()) };
    assert_eq!(at, "\"a\"");
    assert!(matches!(refused.kind, ConstCheckKind::NonConstMatchEq { conditionally: false, .. }));
    assert!(refused.message.contains("`str`"), "{}", refused.message);
}

/// `tests/ui/consts/issue-28113.rs`, the constant named.
#[test]
fn a_closure_call_in_a_constant_names_constants() {
    let src = "const X: u8 =\n    || -> u8 { 5 }()\n;\n";
    let found = check("fn", src, |db| constant(db, "X").into());
    assert_eq!(
        reported(&found),
        [("E0015", "cannot call non-const closure in constants", "|| -> u8 { 5 }()")]
    );
}

/// `tests/ui/traits/const-traits/issue-88155.rs`.
#[test]
fn a_non_const_trait_method_is_named_with_its_self_type() {
    let src = "\
#![feature(const_trait_impl)]

pub trait A {
    fn assoc() -> bool;
}

pub const fn foo<T: A>() -> bool {
    T::assoc()
}
";
    let found = check("sized", src, func("foo"));
    assert_eq!(
        reported(&found),
        [(
            "E0015",
            "cannot call non-const associated function `<T as A>::assoc` in constant functions",
            "T::assoc()",
        )]
    );
    assert!(matches!(
        found[0].0.kind,
        ConstCheckKind::NonConstFn {
            descr: "associated function",
            conditionally: false,
            non_const_trait: Some(_),
            ..
        }
    ));
}

/// `tests/ui/traits/const-traits/const-check-fns-in-const-impl.rs`, the impl written
/// `impl const`: a method of a `const impl` is a const context.
#[test]
fn a_const_impl_method_is_checked() {
    let src = "\
#![feature(const_trait_impl)]

struct S;
const trait T {
    fn foo();
}

fn non_const() {}

impl const T for S {
    fn foo() {
        non_const()
    }
}
";
    let found = check("sized", src, impl_method("foo"));
    assert_eq!(
        reported(&found),
        [("E0015", "cannot call non-const function `non_const` in constant functions", "non_const()")]
    );
}

/// `tests/ui/consts/const-extern-fn/const-extern-fn-call-extern-fn.rs`.
#[test]
fn a_const_extern_fn_may_not_call_a_non_const_one() {
    let src = "\
extern \"C\" {
    fn regular_in_block();
}

const extern \"C\" fn bar() {
    unsafe {
        regular_in_block();
    }
}

extern \"C\" fn regular() {}

const extern \"C\" fn foo() {
    unsafe {
        regular();
    }
}
";
    let message = |name| format!("cannot call non-const function `{name}` in constant functions");
    let bar = check("sized", src, func("bar"));
    assert_eq!(reported(&bar), [("E0015", &*message("regular_in_block"), "regular_in_block()")]);
    let foo = check("sized", src, func("foo"));
    assert_eq!(reported(&foo), [("E0015", &*message("regular"), "regular()")]);
}

/// `tests/ui/consts/issue-16538.rs`: the call is refused in a static, whatever else is wrong
/// with it (the two `E0133` are unsafety's, not this check's).
#[test]
fn a_static_names_statics_and_the_module_path() {
    let src = "\
mod Y {
    pub type X = usize;
    extern \"C\" {
        pub static x: *const usize;
    }
    pub fn foo(value: *const X) -> *const X {
        value
    }
}

static foo: &Y::X = &*Y::foo(Y::x as *const Y::X);
";
    let found =
        check("sized", src, |db| hir_def::test_items::static_item(db, fixture::test_crate(&db), "foo").into());
    assert_eq!(
        reported(&found),
        [(
            "E0015",
            "cannot call non-const function `Y::foo` in statics",
            "Y::foo(Y::x as *const Y::X)",
        )]
    );
}

/// `tests/ui/consts/const-unstable-intrinsic.rs`'s `copy::copy`: a function in a body is a
/// path through the function whose body it is in.
#[test]
fn a_function_in_a_body_is_named_through_its_owner() {
    let src = "\
const fn outer() {
    fn inner() {}
    inner()
}
";
    let found = check("sized", src, func("outer"));
    assert_eq!(
        reported(&found),
        [("E0015", "cannot call non-const function `outer::inner` in constant functions", "inner()")]
    );
}

/// rustc's `def_path_str_with_args`: the impl's type with its arguments, and a function's own.
#[test]
fn a_call_is_named_with_its_generic_arguments() {
    let src = "\
struct V<T>(T);
impl<T> V<T> {
    fn new() {}
}
fn g<T>() {}
const fn by_impl() {
    V::<u8>::new()
}
const fn by_own() {
    g::<u16>()
}
";
    let by_impl = check("sized", src, func("by_impl"));
    assert_eq!(
        reported(&by_impl),
        [(
            "E0015",
            "cannot call non-const associated function `V::<u8>::new` in constant functions",
            "V::<u8>::new()",
        )]
    );
    let by_own = check("sized", src, func("by_own"));
    assert_eq!(
        reported(&by_own),
        [("E0015", "cannot call non-const function `g::<u16>` in constant functions", "g::<u16>()")]
    );
}

/// `tests/ui/consts/const-eval/const_raw_ptr_ops.rs`.
#[test]
fn a_pointer_comparison_is_refused() {
    let src = "\
const X: bool = unsafe { &1 as *const i32 == &2 as *const i32 };
const X2: bool = unsafe { 42 as *const i32 == 43 as *const i32 };
";
    let message = "pointers cannot be reliably compared during const eval";
    let x = check("sized", src, |db| constant(db, "X").into());
    assert_eq!(reported(&x), [("", message, "&1 as *const i32 == &2 as *const i32")]);
    let x2 = check("sized", src, |db| constant(db, "X2").into());
    assert_eq!(reported(&x2), [("", message, "42 as *const i32 == 43 as *const i32")]);
}

/// `tests/ui/consts/const-for-feature-gate.rs`, the constant named: both of the loop's calls,
/// `into_iter` and `next`, are refused.
#[test]
fn a_for_loop_refuses_both_of_its_calls() {
    let src = "const C: () = {\n    for _ in 0..5 {}\n};\n";
    let found = check("iterator, range", src, |db| constant(db, "C").into());
    let desugarings: Vec<_> = found
        .iter()
        .map(|(it, _)| match it.kind {
            ConstCheckKind::NonConstDesugaring { desugaring, conditionally: false, .. } => {
                assert_eq!(it.code, "E0015");
                assert!(it.message.starts_with("cannot use `for` loop on `"), "{}", it.message);
                assert!(it.message.ends_with("` in constants"), "{}", it.message);
                desugaring
            }
            ref other => panic!("not a desugaring's call: {other:?}"),
        })
        .collect();
    assert_eq!(desugarings, [Desugaring::ForLoopIntoIter, Desugaring::ForLoopNext]);
}

/// `tests/ui/consts/const-try-feature-gate.rs`: `?` refuses its `branch` and its
/// `from_residual`, each on `Option<()>`.
#[test]
fn a_question_mark_refuses_both_of_its_calls() {
    let src = "const fn t() -> Option<()> {\n    Some(())?;\n    None\n}\n";
    let found = check("try, option", src, func("t"));
    let desugarings: Vec<_> = found
        .iter()
        .map(|(it, text)| match it.kind {
            ConstCheckKind::NonConstDesugaring { desugaring, conditionally: false, .. } => {
                assert_eq!(it.code, "E0015");
                assert_eq!(it.message, "`?` is not allowed on `Option<()>` in constant functions");
                assert_eq!(text, "Some(())?");
                desugaring
            }
            ref other => panic!("not a desugaring's call: {other:?}"),
        })
        .collect();
    assert_eq!(desugarings, [Desugaring::QuestionBranch, Desugaring::QuestionFromResidual]);
}

/// `tests/ui/traits/const-traits/call-const-trait-method-fail.rs`, the `impl`s written `impl
/// const`: a const trait's method is callable where `[const] Plus` holds, and where it doesn't
/// the error is typeck's `E0277`, not this check's.
#[test]
fn a_const_trait_method_is_callable_where_its_bound_holds() {
    let src = "\
#![feature(const_trait_impl)]

pub const trait Plus {
    fn plus(self, rhs: Self) -> Self;
}

impl const Plus for i32 {
    fn plus(self, rhs: Self) -> Self {
        self + rhs
    }
}

impl Plus for u32 {
    fn plus(self, rhs: Self) -> Self {
        self + rhs
    }
}

pub const fn add_i32(a: i32, b: i32) -> i32 {
    a.plus(b)
}

pub const fn add_u32(a: u32, b: u32) -> u32 {
    a.plus(b)
}
";
    assert_eq!(reported(&check("sized", src, func("add_i32"))), NONE);
    assert_eq!(reported(&check("sized", src, func("add_u32"))), NONE);
}

/// `tests/ui/consts/const-unstable-intrinsic.rs`, the calls of its own crate's intrinsics
/// (the other crate's need its auxiliary crate): a `const fn` exposed to stable may not use
/// what `#![feature(local)]` allows, and a const-stable one may not call an intrinsic that
/// is not `#[rustc_intrinsic_const_stable_indirect]`.
const UNSTABLE_INTRINSIC: &str = "\
#![feature(staged_api, rustc_attrs, intrinsics)]
#![stable(since = \"1.0.0\", feature = \"stable\")]
#![feature(local)]

const fn const_main() {
    let x = 42;
    unsafe {
        size_of_val(&x);
        align_of_val(&x);
    }
}

#[unstable(feature = \"local\", issue = \"42\")]
#[rustc_intrinsic]
pub const unsafe fn size_of_val<T>(x: *const T) -> usize;

#[unstable(feature = \"local\", issue = \"42\")]
#[rustc_const_unstable(feature = \"local\", issue = \"42\")]
#[rustc_intrinsic]
pub const unsafe fn align_of_val<T>(x: *const T) -> usize;

#[stable(feature = \"rust1\", since = \"1.0.0\")]
#[rustc_const_stable(feature = \"const_intrinsic_copy\", since = \"1.63.0\")]
pub const unsafe fn copy<T>(src: *const T, dst: *mut T, count: usize) {
    #[rustc_intrinsic]
    const unsafe fn copy<T>(src: *const T, dst: *mut T, count: usize);

    unsafe { copy(src, dst, count) }
}

mod non_const_fallback {
    #[rustc_intrinsic]
    #[rustc_const_stable_indirect]
    #[rustc_do_not_const_check]
    pub const unsafe fn size_of_val<T>(_x: *const T) -> usize {
        0
    }

    #[stable(feature = \"rust1\", since = \"1.0.0\")]
    #[rustc_const_stable(feature = \"rust1\", since = \"1.0.0\")]
    pub const fn something_stable<T>(x: *const T) -> usize {
        unsafe { size_of_val(x) }
    }
}
";

#[test]
fn an_exposed_const_fn_may_not_use_a_local_feature() {
    let found = check("sized", UNSTABLE_INTRINSIC, func("const_main"));
    let message =
        "const function that might be (indirectly) exposed to stable cannot use `#[feature(local)]`";
    assert_eq!(
        reported(&found),
        [("", message, "size_of_val(&x)"), ("", message, "align_of_val(&x)")]
    );
    for (it, _) in &found {
        assert!(matches!(
            &it.kind,
            ConstCheckKind::UnstableInStableExposed {
                gate,
                is_function_call: false,
                already_unstable: false,
            } if gate.as_str() == "local"
        ));
    }
}

#[test]
fn a_const_stable_fn_may_not_call_an_unmarked_intrinsic() {
    let found = check("sized", UNSTABLE_INTRINSIC, func("copy"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].1, "copy(src, dst, count)");
    assert!(matches!(found[0].0.kind, ConstCheckKind::UnmarkedIntrinsicExposed { .. }));

    let found = check("sized", UNSTABLE_INTRINSIC, func("something_stable"));
    assert_eq!(
        reported(&found),
        [(
            "",
            "intrinsic `non_const_fallback::size_of_val` cannot be (indirectly) exposed to stable",
            "size_of_val(x)",
        )]
    );
}

/// What rustc accepts is accepted: a `const fn` calling a `const fn`, a tuple struct and
/// `panic!` with a literal.
#[test]
fn const_calls_are_accepted() {
    let src = "\
struct W(u32);
const fn one() -> u32 { 1 }
const fn two() -> W {
    if one() == 0 {
        panic!(\"zero\");
    }
    W(one() + one())
}
";
    assert_eq!(reported(&check("panic", src, func("two"))), NONE);
}
