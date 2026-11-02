//! Allocation budgets of the store's hot paths: reading a store, a memo hit of a body or its
//! scopes, a scope lookup, source map lookups both ways, and the syntax side of lowering's
//! walk allocate nothing.

use std::hint::black_box;

use blaze_alloc_count::assert_at_most;
use hir_expand::{HirFileId, InFile};
use span::Edition;
use syntax::{
    AstPtr, SourceFile,
    ast as tag,
    view::{self as ast, AstNode as _},
};
use hir_expand::fixture;

use crate::{
    DefWithBodyId, ModuleDefId,
    expr_store::{
        Body, ExprPtr,
        lower::{comma_follows_token, may_have_attrs},
        scope::ExprScopes,
    },
    nameres::crate_def_map,
    test_db::{TestDB, TestDBExt},
};

const CODE: &str = r#"
struct S { a: u32 }

fn f(s: S, x: u32) -> u32 {
    let (y, z) = (x + 1, [x; 2]);
    let c = |w: u32| w * s.a;
    match y {
        0 => z[0],
        n if n > 3 => c(n),
        _ => {
            let t: &u32 = &y;
            *t
        }
    }
}
"#;

fn the_function(db: &TestDB) -> DefWithBodyId {
    for (_, module) in crate_def_map(db, db.fetch_test_crate()).modules(db) {
        for decl in module.scope(db).declarations() {
            if let ModuleDefId::FunctionId(it) = decl {
                return it.into();
            }
        }
    }
    panic!("no function in the fixture")
}

#[test]
fn reading_a_store_allocates_nothing() {
    let fx = fixture::db(CODE);
    let _ = fx.only_file();
    let db = fx.into_db();
    let body = Body::of(&db, the_function(&db));
    assert!(body.expr_ids().len() > 10 && body.pat_ids().len() > 5, "too small to measure");
    assert_at_most(0, "reading every node of a store", || {
        for expr in body.expr_ids() {
            black_box(body.expr(expr));
        }
        for pat in body.pat_ids() {
            black_box(body.pat(pat));
        }
        for (id, type_ref) in body.type_refs() {
            black_box((id, type_ref));
        }
    });
}

#[test]
fn a_memo_hit_allocates_nothing() {
    let fx = fixture::db(CODE);
    let _ = fx.only_file();
    let db = fx.into_db();
    let def = the_function(&db);
    Body::of(&db, def);
    ExprScopes::of(&db, def);
    assert_at_most(0, "a memo hit of `Body::of`", || black_box(Body::of(&db, def)));
    assert_at_most(0, "a memo hit of `ExprScopes::of`", || black_box(ExprScopes::of(&db, def)));
}

#[test]
fn a_scope_lookup_allocates_nothing() {
    let fx = fixture::db(CODE);
    let _ = fx.only_file();
    let db = fx.into_db();
    let def = the_function(&db);
    let body = Body::of(&db, def);
    let scopes = ExprScopes::of(&db, def);
    let named = body
        .expr_ids()
        .filter_map(|expr| scopes.scope_for(expr))
        .find_map(|scope| scopes.entries(scope).first())
        .expect("a scope with a binding");
    let name = named.name().clone();
    assert_at_most(0, "resolving a name from every expression's scope", || {
        for expr in body.expr_ids() {
            if let Some(scope) = scopes.scope_for(expr) {
                black_box(scopes.resolve_name_in_scope(scope, &name));
                black_box(scopes.scope_chain(Some(scope)).count());
            }
        }
    });
}

/// A warm body's source map, both ways: each expression's, pattern's and type's syntax by id
/// (the back maps), each binding's patterns (`BindingPats`), and each node's id by its syntax
/// (`SourceTable`'s search).
#[test]
fn source_map_lookups_allocate_nothing() {
    let fx = fixture::db(CODE);
    let file = fx.only_file();
    let db = fx.into_db();
    let def = the_function(&db);
    let (body, map) = Body::with_source_map(&db, def);
    let root = file.parse(&db).root();
    let file = HirFileId::from(file);
    let lookups = || {
        let mut found = 0;
        for expr in body.expr_ids() {
            found += usize::from(map.expr_syntax(expr).is_ok());
        }
        for pat in body.pat_ids() {
            found += usize::from(map.pat_syntax(pat).is_ok());
        }
        for (id, _) in body.type_refs() {
            found += usize::from(map.type_syntax(id).is_ok());
        }
        for (binding, _) in body.bindings() {
            found += map.patterns_for_binding(binding).len();
        }
        for node in root.descendants() {
            if let Some(expr) = ast::Expr::cast(node) {
                found += usize::from(map.node_expr_view(InFile::new(file, expr)).is_some());
            }
            if let Some(pat) = ast::Pat::cast(node) {
                found += usize::from(map.node_pat_view(InFile::new(file, pat)).is_some());
            }
            if let Some(ty) = ast::Type::cast(node) {
                found += usize::from(map.node_type_view(InFile::new(file, ty)).is_some());
            }
        }
        found
    };
    let warm = lookups();
    assert!(warm > 40, "too few lookups found something: {warm}");
    let again = assert_at_most(0, "source map lookups both ways", lookups);
    assert_eq!(again, warm);
}

/// What lowering reads of the syntax at each node (the cfg fast path, the source map's
/// pointers, names' text, the trailing-comma check) allocates nothing, over every node.
#[test]
fn the_syntax_side_of_the_walk_allocates_nothing() {
    let parse = SourceFile::parse(CODE, Edition::CURRENT);
    let root = parse.root();
    assert!(root.descendants().count() > 50, "too small to measure");
    assert_at_most(0, "reading every node's syntax as lowering does", || {
        for node in root.descendants() {
            if let Some(owner) = ast::AnyHasAttrs::cast(node) {
                black_box(may_have_attrs(&owner));
            }
            if let Some(expr) = ast::Expr::cast(node) {
                black_box(ExprPtr::of(expr));
            }
            if let Some(pat) = ast::Pat::cast(node) {
                black_box(AstPtr::<tag::Pat>::of(pat));
            }
            if let Some(name) = ast::Name::cast(node) {
                black_box(name.text());
            }
            if let Some(name_ref) = ast::NameRef::cast(node) {
                black_box(name_ref.text());
            }
            black_box(comma_follows_token(node.last_token()));
        }
    });
}
