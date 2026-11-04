//! Fixture tests, one a property: each names what a `#[blaze::linear]` program may and may not
//! do, and checks the errors it gets (the message's key words and the text its span covers).
//! `unmarked_code_gets_no_error` is the opt-in property: code with no mark gets nothing.

use base_db::all_crates;
use hir_def::{AssocItemId, DefWithBodyId, ModuleDefId, expr_store::Body, nameres::crate_def_map};
use hir_expand::fixture;
use hir_ty::{InferBodyId, db::HirDatabase, next_solver::TyKind};
use hir_ty_mir::mir::{AggregateKind, MirBody, MirSpan, Rvalue, StatementKind};
use rustc_type_ir::inherent::IntoKind;

use crate::{BodyKind, LinearDiagnostic, check_body};

use hir_ty_mir as _;

type TestDB = base_db::Db;

/// The functions of every module and impl of every crate.
fn functions(db: &TestDB) -> Vec<DefWithBodyId> {
    let mut found = Vec::new();
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            for def in scope.declarations() {
                if let ModuleDefId::FunctionId(it) = def {
                    found.push(it.into());
                }
            }
            for it in scope.impls() {
                for &(_, item) in it.impl_items(db).items.iter() {
                    if let AssocItemId::FunctionId(it) = item {
                        found.push(it.into());
                    }
                }
            }
        }
    }
    found
}

/// `body` and the closures and coroutines written in it, as the borrow check walks them.
fn with_nested<'db>(db: &'db TestDB, body: &'db MirBody<'db>, out: &mut Vec<&'db MirBody<'db>>) {
    out.push(body);
    for &closure in body.closures.iter() {
        let nested = db.mir_body_for_closure(closure).expect("a closure lowers");
        with_nested(db, nested, out);
    }
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Closure(ty), _)) =
                &statement.kind
                && let TyKind::Coroutine(id, _) = ty.as_ref().kind()
            {
                let nested = db.mir_body_for_coroutine(id.0).expect("a coroutine lowers");
                with_nested(db, nested, out);
            }
        }
    }
}

/// Each error of every body in `src`: its message and the text its span covers, the last
/// character for the end of an expression (a scope's closing brace).
#[track_caller]
fn check(src: &str) -> Vec<(String, String)> {
    let fixture_text = format!(
        "//- minicore: sized, drop, fn, result, option, try, from, future, async_fn\n\
         #![feature(register_tool)]\n#![register_tool(blaze)]\n{src}"
    );
    let fx = fixture::db(&fixture_text);
    let file = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let text = db.file_text(file.file_id(&db)).text(&db).to_owned();
        let mut found = Vec::new();
        for def in functions(&db) {
            let map = Body::source_map(&db, def);
            let at = |span: MirSpan| {
                let range = match span {
                    MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => {
                        map.expr_syntax(expr).ok().map(|it| it.value.text_range())
                    }
                    MirSpan::PatId(pat) => map.pat_syntax(pat).ok().map(|it| it.value.text_range()),
                    MirSpan::BindingId(binding) => map
                        .patterns_for_binding(binding)
                        .first()
                        .and_then(|&pat| map.pat_syntax(pat).ok())
                        .map(|it| it.value.text_range()),
                    MirSpan::SelfParam => map.self_param_syntax().map(|it| it.value.text_range()),
                    _ => None,
                };
                let Some(range) = range else { return format!("{span:?}") };
                let end = usize::from(range.end());
                let start = match span {
                    MirSpan::ExprEnd(_) => end - 1,
                    _ => usize::from(range.start()),
                };
                text[start..end].to_owned()
            };
            let root = db.mir_body(InferBodyId::DefWithBodyId(def)).expect("the body lowers");
            let mut bodies = Vec::new();
            with_nested(&db, root, &mut bodies);
            for (index, body) in bodies.iter().enumerate() {
                let kind = if index == 0 { BodyKind::Item(def) } else { BodyKind::Nested };
                let mut out: Vec<LinearDiagnostic> = Vec::new();
                check_body(&db, body, kind, &mut out);
                found.extend(out.into_iter().map(|it| (it.message, at(it.span))));
            }
        }
        found
    })
}

/// That `found` is one error a want, each with the words and the span text it names.
#[track_caller]
fn expect(found: &[(String, String)], want: &[(&[&str], &str)]) {
    assert_eq!(found.len(), want.len(), "{found:#?}");
    for ((message, at), (words, span)) in found.iter().zip(want) {
        for word in *words {
            assert!(message.contains(word), "`{word}` is not in: {message}");
        }
        assert_eq!(at, span, "{message}");
    }
}

const TXN: &str = "
#[blaze::linear]
struct Txn { id: u32 }

impl Txn {
    #[blaze::consumes]
    fn commit(self) {}
}

#[blaze::consumes]
fn consume<#[blaze::linear] T>(_: T) {}
";

#[test]
fn unmarked_code_gets_no_error() {
    let found = check(
        "
struct Plain { id: u32 }
fn f(cond: bool) -> Option<u32> {
    let a = Plain { id: 1 };
    let mut b = Plain { id: 2 };
    b = Plain { id: 3 };
    if cond { return None; }
    let v = Some(a.id)?;
    let g = move || b.id;
    Some(v + g())
}
fn generic<T>(t: T) -> T { t }
fn uses() { let _ = generic(Plain { id: 0 }); }
async fn later(p: Plain) -> u32 { p.id }
",
    );
    expect(&found, &[]);
}

#[test]
fn consumed_values_get_no_error() {
    let found = check(&format!(
        "{TXN}
fn moved(t: Txn) {{ t.commit() }}
fn destructured(t: Txn) -> u32 {{ let Txn {{ id }} = t; id }}
fn given(t: Txn) {{ consume(t) }}
fn returned() -> Txn {{ Txn {{ id: 0 }} }}
fn branches(t: Txn, c: bool) {{ if c {{ t.commit() }} else {{ consume(t) }} }}
"
    ));
    expect(&found, &[]);
}

#[test]
fn a_value_left_at_the_end_of_its_scope_is_an_error() {
    let found = check(&format!("{TXN}\nfn f() {{\n    let t = Txn {{ id: 0 }};\n}}\n"));
    expect(&found, &[(&["`t`", "`Txn`", "at the end of its scope"], "}")]);
}

#[test]
fn a_value_moved_on_one_branch_only_is_an_error() {
    let found = check(&format!("{TXN}\nfn f(t: Txn, c: bool) {{\n    if c {{ t.commit() }}\n}}\n"));
    expect(&found, &[(&["`t`", "on a path that does not move it"], "}")]);
}

#[test]
fn an_overwritten_value_is_an_error() {
    let found = check(&format!(
        "{TXN}\nfn f() {{\n    let mut t = Txn {{ id: 0 }};\n    t = Txn {{ id: 1 }};\n    t.commit();\n}}\n"
    ));
    expect(&found, &[(&["`t`", "when a new value is assigned over it"], "t = Txn { id: 1 }")]);
}

#[test]
fn an_early_return_is_an_error() {
    let found = check(&format!(
        "{TXN}\nfn f(c: bool) {{\n    let t = Txn {{ id: 0 }};\n    if c {{ return; }}\n    t.commit();\n}}\n"
    ));
    expect(&found, &[(&["`t`", "`return` leaves early"], "return")]);
}

#[test]
fn a_question_mark_is_an_error() {
    let found = check(&format!(
        "{TXN}\nfn f(r: Result<u32, ()>) -> Result<u32, ()> {{\n    let t = Txn {{ id: 0 }};\n    let v = r?;\n    t.commit();\n    Ok(v)\n}}\n"
    ));
    expect(&found, &[(&["`t`", "`?` returns early"], "r?")]);
}

#[test]
fn a_break_is_an_error() {
    let found = check(&format!(
        "{TXN}\nfn f(c: bool) {{\n    loop {{\n        let t = Txn {{ id: 0 }};\n        if c {{ break; }}\n        t.commit();\n    }}\n}}\n"
    ));
    expect(&found, &[(&["`t`", "`break`"], "break")]);
}

#[test]
fn a_field_left_in_a_struct_is_an_error() {
    let found = check(&format!(
        "{TXN}\nstruct Pair {{ a: Txn, b: Txn }}\nfn f(p: Pair) {{\n    p.a.commit();\n}}\n"
    ));
    expect(&found, &[(&["`p.b`", "`Txn`"], "}")]);
}

#[test]
fn unwinding_drops_without_an_error() {
    // `commit` and `may_panic` may unwind with `t` live: that drop is allowed.
    let found = check(&format!(
        "{TXN}\nfn may_panic() {{}}\nfn f() {{\n    let t = Txn {{ id: 0 }};\n    may_panic();\n    t.commit();\n}}\n"
    ));
    expect(&found, &[]);
}

#[test]
fn an_unmarked_type_parameter_refuses_a_linear_argument() {
    let found = check(&format!(
        "{TXN}\nfn keep<T>(t: T) -> T {{ t }}\nfn f() {{\n    keep(Txn {{ id: 0 }}).commit();\n}}\n"
    ));
    expect(
        &found,
        &[(&["`Txn`", "type parameter `T`", "does not admit linear types"], "keep(Txn { id: 0 })")],
    );
}

#[test]
fn a_marked_type_parameter_admits_a_linear_argument_and_is_checked() {
    let found = check(&format!(
        "{TXN}
fn keep<#[blaze::linear] T>(t: T) -> T {{ t }}
fn lose<#[blaze::linear] T>(t: T) {{
    let _kept = t;
}}
fn f() {{
    keep(Txn {{ id: 0 }}).commit();
}}
"
    ));
    expect(&found, &[(&["`_kept`", "`T`", "at the end of its scope"], "}")]);
}

#[test]
fn an_async_body_holding_a_linear_value_is_an_error() {
    let found = check(&format!("{TXN}\nasync fn f(t: Txn) {{\n    t.commit();\n}}\n"));
    // `t` and any temporary the coroutine moves it through: each is named.
    assert!(!found.is_empty(), "no error");
    assert!(found.iter().all(|(message, _)| message.contains("`async` body")), "{found:#?}");
    assert!(found.iter().any(|(message, at)| message.contains("`t`") && at == "t"), "{found:#?}");
}
