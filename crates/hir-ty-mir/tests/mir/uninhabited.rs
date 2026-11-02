//! `MirBody::unreachable_after_calls`, as rustc's `LintAndRemoveUninhabited` finds them
//! (`tests/ui/lint/dead-code/issue-85071.rs` and `issue-85071-2.rs` are the first two): after a
//! call of an uninhabited type other than `!`, the first user code, "definition" at a `let`'s
//! pattern and "expression" elsewhere; none after a `!` call, in a function whose own return
//! type is uninhabited, in a closure, or where only the return value and storage follow; and
//! the kept body still ends at the call.

use hir_def::{DefWithBodyId, expr_store::Body};
use hir_ty::{
    db::HirDatabase,
    mir::{MirSpan, TerminatorKind},
};
use hir_expand::fixture;

use crate::function;

const ITEMS: &str = "\
enum Void {}
fn f() -> Void { loop {} }
fn g() -> u32 { 1 }
fn n() -> ! { loop {} }
struct S;
impl S {
    fn f(&self) -> Void { loop {} }
}
";

/// For the function `name` of `ITEMS` and `src`: each entry's call, what follows it, and its
/// word, as text.
#[track_caller]
fn entries(src: &str, name: &str) -> Vec<(String, String, &'static str)> {
    let text = format!("{ITEMS}{src}");
    let fx = fixture::db(&format!("//- minicore: sized\n{text}"));
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let func = function(&db, name);
        let body =
            db.mir_body(func.into()).unwrap_or_else(|error| panic!("the MIR of `{name}`: {error:?}"));
        let (_, map) = Body::with_source_map(&db, DefWithBodyId::from(func));
        let at = |span: MirSpan| {
            let source = match span {
                MirSpan::ExprId(expr) => map.expr_syntax(expr),
                MirSpan::PatId(pat) => map.pat_syntax(pat),
                other => panic!("a span with no syntax: {other:?}"),
            }
            .unwrap_or_else(|_| panic!("synthetic syntax at {span:?}"));
            let range = source.value.text_range();
            text[usize::from(range.start())..usize::from(range.end())].to_owned()
        };
        body.unreachable_after_calls.iter().map(|it| (at(it.call), at(it.next), it.descr)).collect()
    })
}

fn entry(call: &str, next: &str, descr: &'static str) -> (String, String, &'static str) {
    (call.to_owned(), next.to_owned(), descr)
}

const NONE: [(String, String, &str); 0] = [];

#[test]
fn a_let_of_a_call_is_an_unreachable_definition() {
    let src = "fn main() {\n    let x = f();\n    let _ = x;\n}\n";
    assert_eq!(entries(src, "main"), [entry("f()", "x", "definition")]);
}

#[test]
fn a_let_of_a_method_call_is_an_unreachable_definition() {
    let src = "fn main() {\n    let s = S;\n    let x = s.f();\n    let _y = x;\n}\n";
    assert_eq!(entries(src, "main"), [entry("s.f()", "x", "definition")]);
}

#[test]
fn a_call_after_a_statement_is_an_unreachable_expression() {
    let src = "fn main() {\n    f();\n    g();\n}\n";
    assert_eq!(entries(src, "main"), [entry("f()", "g()", "expression")]);
}

#[test]
fn the_kept_body_ends_at_the_call() {
    let src = "fn main() {\n    f();\n    g();\n}\n";
    let fx = fixture::db(&format!("//- minicore: sized\n{ITEMS}{src}"));
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let body = db.mir_body(function(&db, "main").into()).expect("the MIR of `main`");
        let calls: Vec<_> = body
            .basic_blocks
            .iter()
            .filter_map(|(_, block)| match &block.terminator.as_ref()?.kind {
                TerminatorKind::Call { target, .. } => Some(*target),
                _ => None,
            })
            .collect();
        assert!(matches!(calls[..], [None]), "only `f()` is built, with no target: {calls:?}");
    });
}

#[test]
fn entries_come_in_the_order_of_the_calls_blocks() {
    let src = "\
fn two(b: bool) {
    if b {
        let x = f();
        let _ = x;
    } else {
        f();
        g();
    }
}
";
    assert_eq!(
        entries(src, "two"),
        [entry("f()", "x", "definition"), entry("f()", "g()", "expression")]
    );
}

#[test]
fn none_after_a_never_call() {
    let src = "fn main() {\n    n();\n    let y = 1;\n    let _ = y;\n}\n";
    assert_eq!(entries(src, "main"), NONE);
}

#[test]
fn none_where_the_function_returns_an_uninhabited_type() {
    let src = "fn h() -> Void {\n    let v = f();\n    v\n}\n";
    assert_eq!(entries(src, "h"), NONE);
}

#[test]
fn none_where_only_the_return_value_and_storage_follow() {
    let unit = "fn main() {\n    f();\n}\n";
    assert_eq!(entries(unit, "main"), NONE);
    let value = "fn k() -> u32 {\n    f();\n    5\n}\n";
    assert_eq!(entries(value, "k"), NONE);
}

#[test]
fn none_for_a_call_in_a_closure() {
    let src = "fn main() {\n    let c = || {\n        f();\n        g();\n    };\n    let _ = c;\n}\n";
    assert_eq!(entries(src, "main"), NONE);
}
