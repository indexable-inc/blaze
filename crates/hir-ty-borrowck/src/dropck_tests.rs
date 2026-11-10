//! The drop check's overflow (E0320) as rustc reports it, on two shapes:
//! `param140.rs`, a chain of `let`s each wrapping the last in
//! one more `Option` around a projection rustc cannot see through, and `ident_2000_50.rs`, the
//! same with fifty `Option`s a step. Past the crate's recursion limit (128) the walk of rustc's
//! `dropck_outlives` overflows and each local deeper than that is refused, naming the body and
//! the limit; under `#![recursion_limit = "4096"]` the same body passes.

use base_db::{Db, all_crates};
use hir_def::{
    DefWithBodyId, ModuleDefId,
    expr_store::{Body, BodySourceMap},
    nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use hir_ty::mir::MirSpan;

use crate::BorrowckDiagnostic;

fn function(db: &Db, name: &str) -> DefWithBodyId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it.into();
                }
            }
        }
    }
    panic!("no function `{name}`")
}

/// The source text at `span`, `let ` before it for a binding, so a `let`'s local and the
/// temporary of an argument naming it differ.
fn source(db: &Db, source_map: &BodySourceMap, span: MirSpan) -> String {
    let binding = matches!(span, MirSpan::PatId(_) | MirSpan::BindingId(_));
    let source = match span {
        MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => source_map.expr_syntax(expr),
        MirSpan::PatId(pat) => source_map.pat_syntax(pat),
        MirSpan::BindingId(binding) => {
            source_map.pat_syntax(source_map.patterns_for_binding(binding)[0])
        }
        other => panic!("{other:?} has no source"),
    }
    .unwrap_or_else(|_| panic!("{span:?} has no source"));
    let text = syntax::view::to_node(source.value.syntax_node_ptr(), source.file_id.root(db))
        .text()
        .to_owned();
    match binding {
        true => format!("let {text}"),
        false => text,
    }
}

/// rustc's errors for `lets` steps from `first` on: the `let` of `a{first}`, then for each
/// later step its `let` and the temporary its `wrap` call moves the last local into.
fn rustc_order(first: usize, lets: usize) -> Vec<String> {
    let mut want = vec![format!("let a{first}")];
    for i in first + 1..=lets {
        want.push(format!("let a{i}"));
        want.push(format!("a{}", i - 1));
    }
    want
}

/// The source text of each local of `caller` the drop check refuses, in order, and the limits
/// the errors name; the test fails on any other error.
fn overflows(fixture: &str) -> (Vec<String>, Vec<u32>) {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let def = function(&db, "caller");
        let source_map = Body::source_map(&db, def);
        let result = crate::borrowck(&db, def);
        let errors: Vec<&BorrowckDiagnostic> =
            result.diagnostics.iter().filter(|it| it.code.starts_with('E')).collect();
        let mut locals = Vec::new();
        let mut limits = Vec::new();
        for error in errors {
            assert_eq!(error.code, "E0320", "{error:?}");
            assert!(error.message.contains("`caller`"), "{}", error.message);
            let overflow = error.drop_check.as_ref().expect("E0320 carries its types");
            limits.push(overflow.limit);
            locals.push(source(&db, &source_map, error.span));
        }
        (locals, limits)
    })
}

/// `param140.rs`: `a{i}` is `i` `Option`s around `<T as Tr>::A`.
fn param(depth: usize, limit: Option<u32>) -> String {
    let mut text = String::from("//- minicore: sized, option\n");
    if let Some(limit) = limit {
        text += &format!("#![recursion_limit = \"{limit}\"]\n");
    }
    text += "pub trait Tr { type A; }\n\
             pub fn wrap<T>(x: T) -> Option<T> { Some(x) }\n\
             pub fn caller<T: Tr>(x: T::A) {\n    let a0 = x;\n";
    for i in 1..=depth {
        text += &format!("    let a{i} = wrap(a{});\n", i - 1);
    }
    text + "}\n"
}

/// `ident_2000_50.rs`: each `wrap` adds fifty `Option`s, so `a{i}` is `50 * i` deep.
fn ident(lets: usize, limit: Option<u32>) -> String {
    let mut text = String::from("//- minicore: sized, option\n");
    if let Some(limit) = limit {
        text += &format!("#![recursion_limit = \"{limit}\"]\n");
    }
    let (open, close) = ("Option<".repeat(50), ">".repeat(50));
    let (some, end) = ("Some(".repeat(50), ")".repeat(50));
    text += &format!(
        "pub trait Tr {{ type A; }}\n\
         pub fn wrap<T>(x: T) -> {open}T{close} {{ {some}x{end} }}\n\
         pub fn caller<T: Tr>(x: T::A) {{\n    let a0 = x;\n"
    );
    for i in 1..=lets {
        text += &format!("    let a{i} = wrap(a{});\n", i - 1);
    }
    text + "}\n"
}

/// rustc's 21 errors of `param140.rs`: each `let` from `a130` on, and the argument of each
/// `wrap` after it.
#[test]
fn a_local_nested_past_the_recursion_limit_is_refused() {
    let (locals, limits) = overflows(&param(140, None));
    assert_eq!(locals, rustc_order(130, 140));
    assert!(limits.iter().all(|&it| it == 128), "{limits:?}");
}

/// `#![recursion_limit]` is the limit: 4096 accepts `param140.rs`, and 200 moves the first
/// refusal to `a202`, naming 200, as rustc (pinned nightly) does.
#[test]
fn a_raised_recursion_limit_accepts_the_same_body() {
    assert_eq!(overflows(&param(140, Some(4096))), (vec![], vec![]));
    let (locals, limits) = overflows(&param(204, Some(200)));
    assert_eq!(locals, rustc_order(202, 204));
    assert!(limits.iter().all(|&it| it == 200), "{limits:?}");
}

/// As `ident_2000_50.rs`: rustc refuses `a3` (150 deep), each later `let` and the argument of
/// each `wrap` after it, and nothing before.
#[test]
fn fifty_options_a_step_overflow_at_the_third() {
    let (locals, limits) = overflows(&ident(5, None));
    assert_eq!(locals, rustc_order(3, 5));
    assert!(limits.iter().all(|&it| it == 128), "{limits:?}");
}

/// 4096 accepts the fifty-a-step body; under 200, rustc (pinned nightly) lets `a4`, 200 deep,
/// through and refuses from `a5` on.
#[test]
fn fifty_options_a_step_pass_under_a_raised_limit() {
    assert_eq!(overflows(&ident(5, Some(4096))), (vec![], vec![]));
    let (locals, limits) = overflows(&ident(6, Some(200)));
    assert_eq!(locals, rustc_order(5, 6));
    assert!(limits.iter().all(|&it| it == 200), "{limits:?}");
}
