//! Allocation budgets (`blaze-alloc-count`) of name resolution's reads: once warm, a def-map memo
//! hit, a scope lookup, a path that resolves and a resolver's scope push, lookup and pop each
//! allocate nothing.

use std::hint::black_box;

use base_db::{Crate, relevant_crates};
use blaze_alloc_count::assert_at_most;
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::Symbol;
use hir_expand::fixture;

use crate::{
    DefWithBodyId, ModuleDefId, ModuleId,
    expr_store::{Body, HygieneId, path::Path, scope::ExprScopes},
    nameres::crate_def_map,
    resolver::{HasResolver, ResolveValueResult, ValueNs},
    test_db::TestDB,
};

const CODE: &str = r#"
pub mod a {
    pub mod b {
        pub struct C;
        pub fn g() -> u32 {
            0
        }
    }
    pub use self::b::C as Renamed;
}
pub use a::b;
pub mod wide {
    pub struct W0;
    pub struct W1;
    pub struct W2;
    pub struct W3;
    pub struct W4;
    pub struct W5;
    pub struct W6;
    pub struct W7;
    pub struct W8;
    pub struct W9;
    pub struct W10;
    pub struct W11;
}
pub fn f(x: u32) -> u32 {
    let y = x + 1;
    {
        fn local() -> u32 {
            2
        }
        let z = y * local();
        a::b::g() + z
    }
}
"#;

fn fixture() -> (TestDB, Crate) {
    let fx = fixture::db(CODE);
    let file = fx.only_file();
    let db = fx.into_db();
    let krate = relevant_crates(&db, file.file_id(&db))[0];
    (db, krate)
}

fn name(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

fn path(segments: &[&str]) -> ModPath {
    ModPath::from_segments(PathKind::Plain, segments.iter().map(|it| name(it)))
}

fn module(db: &TestDB, krate: Crate, text: &str) -> ModuleId {
    let map = crate_def_map(db, krate);
    match map.module(db, map.root).scope(db).get(&name(text)).take_types() {
        Some(ModuleDefId::ModuleId(it)) => it,
        other => panic!("`{text}` is not a module of the fixture's root: {other:?}"),
    }
}

fn the_function(db: &TestDB, krate: Crate) -> DefWithBodyId {
    let map = crate_def_map(db, krate);
    map.module(db, map.root)
        .scope(db)
        .declarations()
        .find_map(|it| match it {
            ModuleDefId::FunctionId(it) => Some(it.into()),
            _ => None,
        })
        .expect("the fixture's root declares `f`")
}

/// Budget: a memo hit of `crate_def_map`, of `ModuleId::def_map` and of every block's
/// `block_def_map`: 0 allocations.
#[test]
fn def_map_memo_hits_allocate_nothing() {
    let (db, krate) = fixture();
    let wide = module(&db, krate, "wide");
    let def = the_function(&db, krate);
    let body = Body::of(&db, def);
    assert!(body.blocks(&db).count() >= 1, "the fixture's `f` has no block with items");
    assert_at_most(0, "a memo hit of `crate_def_map`", || black_box(crate_def_map(&db, krate)));
    assert_at_most(0, "a memo hit of `ModuleId::def_map`", || black_box(wide.def_map(&db)));
    assert_at_most(0, "a memo hit of each block's `block_def_map`", || {
        black_box(body.blocks(&db).count())
    });
}

/// Budget: `ItemScope::get` of names that are there and one that is not, in a scope of at most
/// eight names (the linear scan) and one of more (the index), and the same through each
/// module's per-name memo once warm: 0 allocations.
#[test]
fn scope_lookups_allocate_nothing() {
    let (db, krate) = fixture();
    let map = crate_def_map(&db, krate);
    let root = map.module(&db, map.root);
    let wide = map.module(&db, module(&db, krate, "wide"));
    let at_root: Vec<Name> = ["a", "b", "wide", "f", "missing"].map(name).into();
    let in_wide: Vec<Name> =
        // alloc: test setup, outside the measure
        (0..12).map(|i| name(&format!("W{i}"))).chain([name("missing")]).collect();
    assert!(wide.scope(&db).entries().count() > 8, "`wide` does not reach the index");
    assert!(root.scope(&db).get(&name("b")).take_types().is_some(), "`b` is not re-exported");
    assert_at_most(0, "`ItemScope::get` in a small scope and a wide one", || {
        for it in &at_root {
            black_box(root.scope(&db).get(it));
        }
        for it in &in_wide {
            black_box(wide.scope(&db).get(it));
        }
    });
    let lookups = || {
        for it in &at_root {
            black_box(root.name_in_scope(&db, it));
        }
        for it in &in_wide {
            black_box(wide.name_in_scope(&db, it));
        }
    };
    lookups();
    assert_at_most(0, "a memo hit of each module's per-name fact", lookups);
}

/// Budget: a module path that resolves (two and three segments, one through a renaming
/// import), read through a module's resolver once warm: 0 allocations.
#[test]
fn resolved_module_paths_allocate_nothing() {
    let (db, krate) = fixture();
    let resolver = crate_def_map(&db, krate).root.resolver(&db);
    let paths = [path(&["a", "b", "C"]), path(&["a", "Renamed"]), path(&["b", "g"])];
    for it in &paths {
        let found = resolver.resolve_module_path_in_items(&db, it);
        assert!(found.types.is_some() || found.values.is_some(), "{it:?} does not resolve");
    }
    assert_at_most(0, "module paths resolved through the resolver", || {
        for it in &paths {
            black_box(resolver.resolve_module_path_in_items(&db, it));
        }
    });
}

/// Budget: entering the innermost scope of a body (its block, with a block def map, and its
/// `let`s), looking a local, a block's item and a three-segment path up in the value namespace,
/// and leaving again, once the resolver's stack has grown: 0 allocations.
#[test]
fn resolver_scope_push_lookup_and_pop_allocate_nothing() {
    let (db, krate) = fixture();
    let def = the_function(&db, krate);
    let body = Body::of(&db, def);
    let scopes = ExprScopes::of(&db, def);
    let innermost = body
        .expr_ids()
        .max_by_key(|&it| scopes.scope_chain(scopes.scope_for(it)).count())
        .expect("`f` has expressions");
    let (local, item, qualified) = (path(&["z"]), path(&["local"]), path(&["a", "b", "g"]));
    let paths = [Path::BarePath(&local), Path::BarePath(&item), Path::BarePath(&qualified)];
    let mut resolver = def.resolver(&db);
    let guard = resolver.update_to_inner_scope(&db, def, innermost);
    let found: Vec<_> =
        // alloc: test setup, outside the measure
        paths.iter().map(|it| resolver.resolve_path_in_value_ns(&db, *it, HygieneId::ROOT)).collect();
    assert!(
        matches!(found[0], Some(ResolveValueResult::ValueNs(ValueNs::LocalBinding(_)))),
        "`z` is not a local at the innermost expression: {:?}",
        found[0]
    );
    for (it, found) in paths.iter().zip(&found).skip(1) {
        assert!(
            matches!(found, Some(ResolveValueResult::ValueNs(ValueNs::FunctionId(_)))),
            "{it:?} is not a function: {found:?}"
        );
    }
    resolver.reset_to_guard(guard);
    assert_at_most(0, "a scope push, three value lookups and the pop", || {
        let guard = resolver.update_to_inner_scope(&db, def, innermost);
        for it in &paths {
            black_box(resolver.resolve_path_in_value_ns(&db, *it, HygieneId::ROOT));
        }
        resolver.reset_to_guard(guard);
    });
}

/// Budget: a resolver for a function, once its def map, generics and scopes are warm: 0
/// (`Resolver::scopes` holds its first eight scopes inline).
#[test]
fn a_function_resolver_allocates_nothing() {
    let (db, krate) = fixture();
    let def = the_function(&db, krate);
    black_box(def.resolver(&db));
    assert_at_most(0, "a function's resolver", || black_box(def.resolver(&db)));
}
