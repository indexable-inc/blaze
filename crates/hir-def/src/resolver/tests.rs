//! Allocation budgets of a resolver's walks at the innermost scope of a warm body: a lookup of
//! each kind of name in the type and value namespaces, the traits in scope through a visitor, and
//! the names in scope into a kept map.

use std::hint::black_box;

use blaze_alloc_count::assert_at_most;
use blaze_hash::HashSet;
use hir_expand::fixture;
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::Symbol;

use crate::{
    AssocItemId, DefWithBodyId, TraitId,
    expr_store::{Body, HygieneId, path::Path, scope::ExprScopes},
    hir::ExprId,
    nameres::{assoc::ImplItems, crate_def_map},
    resolver::{HasResolver, NamesInScope, ResolveValueResult, TypeNs, ValueNs},
    test_db::{TestDB, TestDBExt},
};

const CODE: &str = r#"
//- /main.rs crate:main deps:pre
#[prelude_import]
use ::pre::prelude::*;
pub mod m {
    pub struct S;
    impl S {
        pub fn new() -> S {
            S
        }
    }
    pub enum E {
        A,
    }
}
pub trait Tr {
    fn go<T: Copy>(&self, x: T) -> T;
}
pub struct B;
fn f<U>(x: U) -> U {
    x
}
impl Tr for m::S {
    fn go<T: Copy>(&self, x: T) -> T {
        let y = x;
        {
            trait Local {}
            f(y)$0
        }
    }
}
//- /pre.rs crate:pre
pub mod prelude {
    pub struct Pre;
    pub trait PreTr {}
}
"#;

fn name(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

fn path(segments: &[&str]) -> ModPath {
    ModPath::from_segments(PathKind::Plain, segments.iter().map(|it| name(it)))
}

/// The database and the impl's `go`.
fn fixture() -> (TestDB, DefWithBodyId) {
    let fx = fixture::db(CODE);
    let position = fx.position();
    let db = fx.into_db();
    let krate = db.module_at_position(position).krate(&db);
    let map = crate_def_map(&db, krate);
    let impl_ = map
        .module(&db, map.root)
        .scope(&db)
        .impls()
        .next()
        .expect("the fixture's root has `impl Tr for m::S`");
    let go = ImplItems::of(&db, impl_)
        .0
        .items
        .iter()
        .find_map(|&(_, it)| match it {
            AssocItemId::FunctionId(it) => Some(it),
            _ => None,
        })
        .expect("the impl has `go`");
    (db, go.into())
}

/// The expression of `def` with the most scopes around it: `f(y)` in the block.
fn innermost(db: &TestDB, def: DefWithBodyId) -> ExprId {
    let body = Body::of(db, def);
    let scopes = ExprScopes::of(db, def);
    body.expr_ids()
        .max_by_key(|&it| scopes.scope_chain(scopes.scope_for(it)).count())
        .expect("`go` has expressions")
}

fn the_trait(resolver: &crate::resolver::Resolver<'_>, db: &TestDB, text: &str) -> TraitId {
    match resolver.resolve_path_in_type_ns_fully(db, Path::BarePath(&path(&[text]))) {
        Some(TypeNs::TraitId(it)) => it,
        other => panic!("`{text}` is not a trait in scope: {other:?}"),
    }
}

/// Budget: at the innermost scope of a warm body, a type-namespace lookup of a generic
/// parameter, a two-segment path, a root item, a prelude item, a builtin, an enum variant, a
/// module's trait and a block's trait, and a value-namespace lookup of a `let`, a parameter, a
/// root function, an associated item left partial, an enum variant and a unit struct: 0
/// allocations.
#[test]
fn type_and_value_lookups_at_a_warm_scope_allocate_nothing() {
    let (db, def) = fixture();
    let at = innermost(&db, def);
    let mut resolver = def.resolver(&db);
    let _guard = resolver.update_to_inner_scope(&db, def, at);

    let type_paths =
        [&["T"][..], &["m", "S"], &["B"], &["Pre"], &["u32"], &["m", "E", "A"], &["Tr"], &["Local"]]
            .map(path);
    let types = type_paths.each_ref().map(Path::BarePath);
    let found: Vec<_> = types.iter().map(|it| resolver.resolve_path_in_type_ns(&db, *it)).collect();
    let kinds: Vec<&str> = found
        .iter()
        .map(|it| match it {
            Some((TypeNs::GenericParam(_), None, _)) => "generic param",
            Some((TypeNs::AdtId(_), None, _)) => "adt",
            Some((TypeNs::BuiltinType(_), None, _)) => "builtin",
            Some((TypeNs::EnumVariantId(_), None, _)) => "variant",
            Some((TypeNs::TraitId(_), None, _)) => "trait",
            other => panic!("an unexpected type-namespace result: {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        ["generic param", "adt", "adt", "adt", "builtin", "variant", "trait", "trait"]
    );

    let value_paths = [&["y"][..], &["x"], &["f"], &["m", "S", "new"], &["m", "E", "A"], &["B"]]
        .map(path);
    let values = value_paths.each_ref().map(Path::BarePath);
    let found: Vec<_> = values
        .iter()
        .map(|it| resolver.resolve_path_in_value_ns(&db, *it, HygieneId::ROOT))
        .collect();
    assert!(
        matches!(
            &found[..],
            [
                Some(ResolveValueResult::ValueNs(ValueNs::LocalBinding(_))),
                Some(ResolveValueResult::ValueNs(ValueNs::LocalBinding(_))),
                Some(ResolveValueResult::ValueNs(ValueNs::FunctionId(_))),
                Some(ResolveValueResult::Partial(TypeNs::AdtId(_), _)),
                Some(ResolveValueResult::ValueNs(ValueNs::EnumVariantId(_))),
                Some(ResolveValueResult::ValueNs(ValueNs::StructId(_))),
            ]
        ),
        "unexpected value-namespace results: {found:?}"
    );

    assert_at_most(0, "type and value lookups at the innermost scope", || {
        for it in &types {
            black_box(resolver.resolve_path_in_type_ns(&db, *it));
        }
        for it in &values {
            black_box(resolver.resolve_path_in_value_ns(&db, *it, HygieneId::ROOT));
        }
    });
}

/// Budget: the traits in scope at the innermost scope (a block's trait, the impl's trait, the
/// prelude's), walked through a visitor, asked for one trait, and added to a kept set once it
/// has grown: 0 allocations.
#[test]
fn the_traits_in_scope_through_a_visitor_allocate_nothing() {
    let (db, def) = fixture();
    let at = innermost(&db, def);
    let mut resolver = def.resolver(&db);
    let _guard = resolver.update_to_inner_scope(&db, def, at);
    let all = resolver.traits_in_scope(&db);
    let [tr, local, pre] = ["Tr", "Local", "PreTr"].map(|it| the_trait(&resolver, &db, it));
    for it in [tr, local, pre] {
        assert!(all.contains(&it), "{it:?} is not among the traits in scope: {all:?}");
    }
    let mut kept = HashSet::default();
    resolver.traits_in_scope_into(&db, &mut kept);

    let (visited, has) = assert_at_most(0, "the traits in scope through a visitor", || {
        let mut visited = 0;
        resolver.for_each_trait_in_scope(&db, |_| visited += 1);
        let has = resolver.has_trait_in_scope(&db, pre);
        kept.clear();
        resolver.traits_in_scope_into(&db, &mut kept);
        (visited, has)
    });
    assert!(has);
    assert!(visited >= all.len());
    assert_eq!(kept, all);
}

/// Budget: the names in scope at the innermost scope into a map kept from an earlier call: 0
/// allocations (no name of the fixture has two meanings in one scope).
#[test]
fn the_names_in_scope_into_a_kept_map_allocate_nothing() {
    let (db, def) = fixture();
    let at = innermost(&db, def);
    let mut resolver = def.resolver(&db);
    let _guard = resolver.update_to_inner_scope(&db, def, at);
    let fresh = resolver.names_in_scope(&db);
    for it in ["y", "x", "T", "Local", "f", "m", "Pre", "u32"] {
        assert!(fresh.contains_key(&name(it)), "`{it}` is not among the names in scope");
    }
    let mut kept = NamesInScope::default();
    resolver.names_in_scope_into(&db, &mut kept);
    assert_at_most(0, "the names in scope into a kept map", || {
        resolver.names_in_scope_into(&db, &mut kept);
    });
    assert!(kept.iter().eq(fresh.iter()));
}
