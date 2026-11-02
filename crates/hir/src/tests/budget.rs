//! Allocation budgets of `Semantics`' hit paths and the accessors' iterator forms: none once
//! the thread's buffers have grown and the database is warm.

use base_db::EditionedFileId;
use blaze_alloc_count::assert_at_most;
use syntax::{SyntaxKind, SyntaxNodePtr, TextRange, view};
use hir_expand::fixture;

use super::db::TestDB;
use crate::{Adt, Crate, ModuleDef, Variant, semantics::view::Semantics};

const FIXTURE: &str = r#"
//- /main.rs crate:main
macro_rules! id { ($($t:tt)*) => { $($t)* }; }
mod inner {
    pub struct Point { pub x: u32, pub y: u32 }
    pub enum Shape { Dot(Point), Line(Point, Point), Empty }
    pub trait Area { fn area(&self) -> u32; }
    impl Area for Point { fn area(&self) -> u32 { 0 } }
}
use inner::{Point, Shape};
fn origin() -> Point { Point { x: 0, y: 0 } }
fn main() {
    id! { let p = origin(); }
    let _s = Shape::Dot(p);
}
"#;

/// What the tests ask `Semantics` about, found once through the file's views (outside every
/// measured closure) and kept as ranges and pointers, which outlive the views.
struct Targets {
    origin: TextRange,
    p_in_macro: TextRange,
    dot_path: SyntaxNodePtr,
    point_ty: SyntaxNodePtr,
    strukt: SyntaxNodePtr,
    field: SyntaxNodePtr,
    module: SyntaxNodePtr,
}

/// Whether `path`'s last segment is the identifier `text`.
fn ends_in(path: view::Path<'_>, text: &str) -> bool {
    path.segment()
        .and_then(|it| it.name_ref())
        .and_then(|it| it.ident_token())
        .is_some_and(|it| it.text() == text)
}

fn targets(db: &TestDB, file_id: EditionedFileId) -> Targets {
    use view::AstNode as _;
    let parse = file_id.parse(db);
    let root = parse.root();
    let ident = |at: view::Node<'_>, text: &str| {
        at.descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::IDENT && it.text() == text)
            .unwrap_or_else(|| panic!("no identifier `{text}` in the fixture"))
            .text_range()
    };
    let first = |what: &str, found: Option<view::Node<'_>>| {
        view::ptr(found.unwrap_or_else(|| panic!("no {what} in the fixture")))
    };
    let token_tree = root
        .descendants()
        .find_map(view::MacroCall::cast)
        .and_then(|call| call.token_tree())
        .expect("the call `id!`");
    let strukt = root.descendants().find_map(view::Struct::cast).expect("`struct Point`");
    Targets {
        origin: ident(root, "origin"),
        p_in_macro: ident(token_tree.syntax(), "p"),
        dot_path: first(
            "path `Shape::Dot`",
            root.descendants()
                .filter_map(view::Path::cast)
                .find(|it| ends_in(*it, "Dot") && it.qualifier().is_some())
                .map(|it| it.syntax()),
        ),
        point_ty: first(
            "return type `Point`",
            root.descendants()
                .filter_map(view::PathType::cast)
                .find_map(|it| it.path())
                .map(|it| it.syntax()),
        ),
        strukt: view::ptr(strukt.syntax()),
        field: first(
            "field `x`",
            strukt.syntax().descendants().find(|it| view::RecordField::can_cast(it.kind())),
        ),
        module: first(
            "`mod inner`",
            root.descendants().find(|it| view::Module::can_cast(it.kind())),
        ),
    }
}

fn token_at<'db>(file: view::SourceFile<'db>, range: TextRange) -> view::Token<'db> {
    use view::AstNode as _;
    file.syntax().covering_element(range).into_token().expect("a token's range covers a token")
}

fn node_at<'db, N: view::AstNode<'db>>(file: view::SourceFile<'db>, ptr: SyntaxNodePtr) -> N {
    use view::AstNode as _;
    N::cast(view::to_node(ptr, file.syntax())).expect("a pointer's kind is the node's")
}

/// Runs `f` once to warm the database, the thread's shelves and the carried `Semantics`
/// tables, then again under a budget of no allocation, each time with a fresh `Semantics` (a
/// request), as an IDE does.
fn warm_then_free<R>(db: &TestDB, what: &str, mut f: impl FnMut(&Semantics<'_>) -> R) {
    {
        let sema = Semantics::new(db);
        drop(f(&sema));
    }
    {
        let sema = Semantics::new(db);
        drop(f(&sema));
    }
    let sema = assert_at_most(0, &format!("{what}: making a `Semantics`"), || Semantics::new(db));
    let result = assert_at_most(0, what, || f(&sema));
    drop(result);
    assert_at_most(0, &format!("{what}: dropping the `Semantics`"), || drop(sema));
}

#[test]
fn descend_into_macros_on_a_warm_file_allocates_nothing() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let at = targets(&db, file_id);
        warm_then_free(&db, "descend_into_macros_exact", |sema| {
            let file = sema.parse(file_id);
            let origin = sema.descend_into_macros_exact(token_at(file, at.origin));
            assert!(!origin.is_empty());
            let in_macro = sema.descend_into_macros_exact(token_at(file, at.p_in_macro));
            assert_eq!(in_macro.len(), 1, "`p` maps to one token of the expansion");
        });
    });
}

#[test]
fn resolve_path_through_semantics_on_a_warm_file_allocates_nothing() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let at = targets(&db, file_id);
        warm_then_free(&db, "resolve_path", |sema| {
            let file = sema.parse(file_id);
            let path: view::Path<'_> = node_at(file, at.dot_path);
            assert!(sema.resolve_path(path).is_some(), "`Shape::Dot` resolves");
            let ty_path: view::Path<'_> = node_at(file, at.point_ty);
            assert!(sema.resolve_path(ty_path).is_some(), "`Point` resolves");
        });
    });
}

#[test]
fn to_def_on_a_cached_node_allocates_nothing() {
    let fx = fixture::db(FIXTURE);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let at = targets(&db, file_id);
        warm_then_free(&db, "to_def", |sema| {
            let file = sema.parse(file_id);
            let strukt: view::Struct<'_> = node_at(file, at.strukt);
            let def = sema.to_struct_def(strukt).expect("`Point` has a def");
            let field: view::RecordField<'_> = node_at(file, at.field);
            assert!(sema.to_def(field).is_some(), "`x` has a def");
            let module: view::Module<'_> = node_at(file, at.module);
            assert!(sema.to_module_def(module).is_some(), "`inner` has a def");
            def
        });
    });
}

#[test]
fn the_accessors_iterator_forms_allocate_nothing() {
    let db = fixture::db(FIXTURE).into_db();
    crate::attach_db(&db, || {
        let krate = Crate::all(&db).into_iter().next().expect("a crate");
        let root = krate.root_module(&db);
        let inner = root.children(&db).next().expect("`mod inner`");
        let point = inner
            .declarations(&db)
            .into_iter()
            .find_map(|it| match it {
                ModuleDef::Adt(Adt::Struct(it)) => Some(it),
                _ => None,
            })
            .expect("`Point`");
        let shape = inner
            .declarations(&db)
            .into_iter()
            .find_map(|it| match it {
                ModuleDef::Adt(Adt::Enum(it)) => Some(it),
                _ => None,
            })
            .expect("`Shape`");
        let area = inner
            .declarations(&db)
            .into_iter()
            .find_map(|it| match it {
                ModuleDef::Trait(it) => Some(it),
                _ => None,
            })
            .expect("`Area`");
        // Warm: every table read below is computed once here.
        let warm = |db: &TestDB| {
            (
                root.children(db).count(),
                inner.declarations_iter(db).count(),
                inner.impl_defs_iter(db).count(),
                inner.legacy_macros_iter(db).count(),
                point.fields_iter(db).count(),
                Variant::Struct(point).fields_iter(db).count(),
                shape.variants_iter(db).map(|v| v.fields_iter(db).count()).sum::<usize>(),
                area.items_iter(db).count(),
                inner.impl_defs_iter(db).map(|it| it.items_iter(db).count()).sum::<usize>(),
                krate.modules_iter(db).count(),
            )
        };
        let expected = warm(&db);
        let (children, decls, impls, _legacy, fields, variant_fields, shape_fields, items, impl_items, modules) =
            expected;
        assert_eq!((children, decls, impls), (1, 3, 1), "`inner` declares Point, Shape, Area");
        assert_eq!((fields, variant_fields, shape_fields), (2, 2, 3));
        assert_eq!((items, impl_items, modules), (1, 1, 2));
        let again = assert_at_most(0, "the accessors' iterator forms", || warm(&db));
        assert_eq!(again, expected);
        // The `Vec` forms are the same items, in the same order.
        assert_eq!(inner.declarations_iter(&db).collect::<Vec<_>>(), inner.declarations(&db));
        assert_eq!(point.fields_iter(&db).collect::<Vec<_>>(), point.fields(&db));
        assert_eq!(shape.variants_iter(&db).collect::<Vec<_>>(), shape.variants(&db));
        assert_eq!(area.items_iter(&db).collect::<Vec<_>>(), area.items(&db));
    });
}

#[test]
fn a_warm_completion_search_allocates_nothing() {
    let db = fixture::db(
        r#"
//- /main.rs crate:main deps:dep
fn main() {}
//- /dep.rs crate:dep
pub struct Needle;
pub fn needle_fn() {}
pub mod needles { pub struct Inner; }
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let main = Crate::all(&db)
            .into_iter()
            .find(|it| it.display_name(&db).is_some_and(|it| it.crate_name().as_str() == "main"))
            .expect("the crate `main`");
        let search = |query: hir_def::import_map::Query| {
            main.query_external_importables(&db, query).count()
        };
        let first = search(hir_def::import_map::Query::new("needle".to_owned()));
        assert!(first >= 2, "`Needle` and `needle_fn` are found");
        assert_eq!(search(hir_def::import_map::Query::new("needle".to_owned())), first);
        // `Query::new` lowercases into a `String`: the request's edge, made outside the budget.
        let query = hir_def::import_map::Query::new("needle".to_owned());
        let again = assert_at_most(0, "a warm completion search", || search(query));
        assert_eq!(again, first);
        // The same items in the same order on every search.
        let order = |db: &TestDB| {
            main.query_external_importables(db, hir_def::import_map::Query::new("needle".to_owned()))
                .map(|(item, _)| item)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&db), order(&db));
    });
}

#[test]
fn kmerge2_is_kmerge_by() {
    use itertools::Itertools as _;
    let cases: [(&[u32], &[u32]); 6] =
        [(&[], &[]), (&[1, 3], &[]), (&[], &[2]), (&[1, 3, 5], &[2, 3, 4]), (&[2, 2], &[2, 2]), (&[5], &[1, 2, 9])];
    for (left, right) in cases {
        // Tag each item with its side, so a tie's order is checked too.
        let tagged = |side: u8, items: &'static [u32]| items.iter().map(move |&it| (it, side));
        let less = |a: &(u32, u8), b: &(u32, u8)| a.0 < b.0;
        let ours: Vec<_> =
            crate::semantics::kmerge2([tagged(0, left), tagged(1, right)].into_iter(), less).collect();
        let theirs: Vec<_> = [tagged(0, left), tagged(1, right)].into_iter().kmerge_by(less).collect();
        assert_eq!(ours, theirs, "{left:?} and {right:?}");
    }
}

#[test]
fn recycle_keeps_the_allocation() {
    let mut kept: Vec<&'static str> = Vec::with_capacity(16);
    kept.push("x");
    let ptr = kept.as_ptr().cast::<u8>();
    let local = String::from("y");
    let mut lent: Vec<&str> = assert_at_most(0, "recycle", || crate::scratch::recycle(kept));
    assert_eq!(lent.as_ptr().cast::<u8>(), ptr, "the same allocation");
    assert!(lent.capacity() >= 16 && lent.is_empty());
    lent.push(&local);
    let back: Vec<&'static str> = assert_at_most(0, "recycle back", || crate::scratch::recycle(lent));
    assert_eq!(back.as_ptr().cast::<u8>(), ptr);
}
