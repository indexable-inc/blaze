//! The store's synthesized expressions ([`ExpressionStore::is_synthesized`]) against the source
//! map's: a fact of the lowered body that a step reads without reading a position.

use super::*;
use crate::hir::ExprId;

/// Bodies whose lowering makes expressions the source does not write, each kind of desugaring
/// once: a `for` loop, `?`, `async` blocks, closures and `.await`, an `async fn`'s parameter
/// copies, `let else`, `format_args!`, a destructuring assignment, a missing operand, a
/// body a macro writes, and a const's and a static's.
const DESUGARINGS: &str = r#"
//- minicore: iterator, range, try, option, result, fmt, future, async_fn, fn
struct S { a: i32 }
fn loops(n: i32) -> i32 { let mut t = 0; for i in 0..n { t += i; } while t > 9 { t -= 1; } t }
fn question(r: Option<i32>) -> Option<i32> { let v = r?; Some(v + 1) }
async fn later(n: i32, (a, _): (i32, i32), ref mut c: i32) -> i32 { n + a + *c }
async fn awaits() -> i32 { let f = async move { later(1, (2, 3), 4).await }; f.await + 1 }
fn closures(n: i32) -> i32 { let f = |x: i32| x + n; let g = async || n; let _ = g; f(1) }
fn let_else(o: Option<i32>) -> i32 { let Some(v) = o else { return 0; }; v }
fn formats(a: i32, s: &str) { let _ = format_args!("{a} {} {s:?} {:>5}", a + 1, s); }
fn assigns(mut a: i32, mut b: i32, s: &mut S) { (a, b) = (b, a); S { a, .. } = S { a: b }; s.a = a; }
fn missing() -> i32 { let x = 1 + ; x }
macro_rules! written { ($name:ident) => { fn $name(o: Option<i32>) -> Option<i32> { for _ in 0..2 {} Some(o? + 1) } }; }
written!(by_macro);
const C: i32 = { let mut t = 0; for i in 0..3 { t += i; } t };
static T: Option<i32> = { match Some(1) { Some(x) => Some(x), None => None } };
"#;

/// Every body of the fixture's crate: its functions, consts and statics.
fn bodies(db: &TestDB) -> Vec<DefWithBodyId> {
    let def_map = crate_def_map(db, db.fetch_test_crate());
    let mut defs: Vec<DefWithBodyId> = Vec::new(); // alloc: test
    for (_, module) in def_map.modules(db) {
        for decl in module.scope(db).declarations() {
            match decl {
                ModuleDefId::FunctionId(it) => defs.push(it.into()),
                ModuleDefId::ConstId(it) => defs.push(it.into()),
                ModuleDefId::StaticId(it) => defs.push(it.into()),
                _ => {}
            }
        }
    }
    defs
}

/// The expressions of `def`'s body each of its source maps (moved to its file, and the one
/// relative to its anchor when there is one) has no syntax for, beside the store's
/// synthesized ones: the first of each pair is the store's.
pub(super) fn synthesized_beside_the_maps(
    db: &TestDB,
    def: DefWithBodyId,
) -> Vec<(&'static str, Vec<ExprId>, Vec<ExprId>)> {
    let body = Body::of(db, def);
    let stored = body.store.synthesized_exprs().to_vec(); // alloc: test
    let unwritten = |map: &BodySourceMap| -> Vec<ExprId> {
        body.expr_ids().filter(|&expr| map.expr_syntax(expr).is_err()).collect() // alloc: test
    };
    let mut out = vec![("source_map", stored.clone(), unwritten(Body::source_map(db, def)))]; // alloc: test
    if let Some(relative) = Body::relative(db, def) {
        out.push(("relative", stored, unwritten(relative)));
    }
    out
}

/// [`synthesized_beside_the_maps`] of `def`, each pair equal.
pub(super) fn assert_synthesized_is_the_maps(db: &TestDB, def: DefWithBodyId) {
    for (map, stored, unwritten) in synthesized_beside_the_maps(db, def) {
        assert_eq!(
            stored, unwritten,
            "{def:?}: the store's synthesized expressions (left) are not those `{map}` has no syntax for (right)"
        );
    }
}

/// The refinement: for every body of [`DESUGARINGS`] (and, through `round_trip`, of every
/// other lowering test of this file's parent), the expressions the store lists as synthesized
/// are exactly those its source map, moved or relative, has no syntax for
/// (`{ expr | source_map.expr_syntax(expr).is_err() }`), so a step that asks the store reads
/// the same answer the map gave, with no position in it.
#[test]
fn the_synthesized_expressions_are_those_the_source_map_has_no_syntax_for() {
    let db = fixture::db(DESUGARINGS).into_db();
    let defs = bodies(&db);
    assert_eq!(defs.len(), 12, "{defs:?}");
    assert!(
        defs.iter().any(|it| it.erased_ast_id(&db).file_id.macro_file().is_some()),
        "no body a macro writes"
    );
    let mut some: Vec<&str> = Vec::new(); // alloc: test
    for &def in &defs {
        assert_synthesized_is_the_maps(&db, def);
        if !Body::of(&db, def).store.synthesized_exprs().is_empty()
            && let DefWithBodyId::FunctionId(it) = def
        {
            some.push(crate::signatures::FunctionSignature::of(&db, it).name.as_str());
        }
        round_trip(&db, def);
    }
    // The bodies whose lowering allocates an expression no syntax is given: an `async fn`'s and
    // an `async` closure's coroutine and its block (`desugared_coroutine_expr`), `format_args!`'s
    // template and arguments, and the missing right operand of `1 +`. A `for` loop's, a `?`'s and
    // a `while`'s parts each map back to the syntax they stand for, so they are not here.
    for name in ["later", "awaits", "closures", "formats", "missing"] {
        assert!(
            some.contains(&name),
            "`{name}` has no synthesized expression: the fixture lost its desugarings ({some:?} have one)"
        );
    }
}

/// The control: with the planted bug (the desugared expressions lowering maps to the syntax
/// they stand for, a `for` loop's `into_iter`, `iter` and `next`, given that syntax without
/// being recorded as written), the store lists them as synthesized while the map has their
/// syntax, and the refinement's oracle sees it.
#[test]
fn the_oracle_catches_a_desugared_expression_given_syntax_unrecorded() {
    use crate::expr_store::lower::planted::DESUGARED_SYNTAX_FORGOTTEN;

    DESUGARED_SYNTAX_FORGOTTEN.set(true);
    let db = fixture::db(DESUGARINGS).into_db();
    let caught = bodies(&db)
        .into_iter()
        .flat_map(|def| synthesized_beside_the_maps(&db, def))
        .filter(|(_, stored, unwritten)| stored != unwritten)
        .count();
    DESUGARED_SYNTAX_FORGOTTEN.set(false);
    assert!(caught > 0, "the planted bug left every body's synthesized expressions the map's");
}
