//! [`BodyAnalysis`] by pointers into the body's own tree: types, resolutions in the scope of
//! the block a node is in (the body's store, the item's signature, an item nested in the body),
//! what a call calls, where an expression is written, and which locals the body moves.

use base_db::Db;
use hir_def::{ItemContainerId, Lookup as _, hir::ExprId};
use hir_expand::{HirFileId, InFile, name::Name};
use syntax::{
    AstNode, AstPtr, SyntaxKind, ast,
    view::{self, AstNode as _, Typed},
};
use hir_expand::fixture;

use super::BodyAnalysis;
use crate::{
    Crate, DefWithBody, DisplayTarget, Function, HirDisplay, Local, ModuleDef, PathResolution,
    Type,
};

const BODY: &str = r#"
//- minicore: sized, copy, clone, add
//- /main.rs crate:main
struct Plain;
struct Wrap(u8);
impl Wrap { fn get(&self) -> u8 { self.0 } }
trait Show { fn show(&self) -> u8; }
impl Show for Wrap { fn show(&self) -> u8 { self.0 } }
macro_rules! twice { ($e:expr) => { $e + $e }; }
fn takes(v: Plain, r: &Plain, n: u8) -> u8 {
    struct Inner(Plain);
    let w = Wrap(n);
    let a = w.get();
    let b = w.show();
    let c = twice!(n);
    let moved: Plain = v;
    let seen = r;
    a + b + c
}
fn captures(p: Plain, q: Plain) {
    let f = move || p;
    let g = || &q;
}
macro_rules! made { () => { fn made(p: Plain) -> Plain { p } }; }
made!();
"#;

fn func(db: &Db, name: &str) -> Function {
    let found = Crate::all(db)
        .into_iter()
        .flat_map(|krate| krate.root_module(db).declarations_iter(db))
        .find(|it| it.name(db).is_some_and(|it| it.as_str() == name));
    match found {
        Some(ModuleDef::Function(it)) => it,
        it => panic!("`{name}` is {it:?}"),
    }
}

fn target(db: &Db, name: &str) -> DisplayTarget {
    func(db, name).module(db).krate(db).to_display_target(db)
}

fn show<'db>(db: &'db Db, ty: &Type<'db>, target: DisplayTarget) -> String {
    ty.display(db, target).to_string()
}

/// The first `N` of the body's tree whose text is `text`, and a pointer to it.
fn find<'t, N: AstNode + Typed>(
    root: view::Node<'t>,
    file: HirFileId,
    text: &str,
) -> (N::Of<'t>, InFile<AstPtr<N>>)
where
    N::Of<'t>: Copy,
{
    let node = root
        .descendants()
        .filter_map(<N::Of<'t> as view::AstNode<'t>>::cast)
        .find(|it| it.syntax().text() == text)
        .unwrap_or_else(|| panic!("no `{text}` in {root}"));
    let ptr = InFile::new(file, AstPtr::of(node));
    (node, ptr)
}

fn local<'db>(analysis: &BodyAnalysis<'db>, name: &str) -> Local<'db> {
    let (root, file) = analysis.tree();
    let (_, pat) = find::<ast::IdentPat>(root, file, name);
    analysis.binding(pat).unwrap_or_else(|| panic!("`{name}` is no binding"))
}

#[test]
fn the_tree_is_the_body_s_own_and_types_are_read_off_it() {
    let db = fixture::db(BODY).into_db();
    crate::attach_db(&db, || {
        let analysis = BodyAnalysis::new(&db, DefWithBody::from(func(&db, "takes")));
        let target = target(&db, "takes");
        let (root, file) = analysis.tree();
        assert!(matches!(file, HirFileId::FileId(_)), "{file:?}");
        assert!(root.text().starts_with("fn takes"), "{root}");

        let (_, get) = find::<ast::Expr>(root, file, "w.get()");
        let info = analysis.type_of_expr(get).expect("a type of `w.get()`");
        assert_eq!(show(&db, &info.original, target), "u8");

        let (_, w) = find::<ast::Pat>(root, file, "w");
        let info = analysis.type_of_pat(w).expect("a type of `w`");
        assert_eq!(show(&db, &info.original, target), "Wrap");

        let (_, receiver) = find::<ast::Expr>(root, file, "w");
        let adjustments = analysis.expr_adjustments(receiver).expect("`w` is borrowed for `get`");
        let shown: Vec<_> = adjustments.iter().map(|it| show(&db, &it.target, target)).collect();
        assert_eq!(shown, ["&Wrap"]);
    });
}

#[test]
fn a_method_call_resolves_to_the_impl_s_function() {
    let db = fixture::db(BODY).into_db();
    crate::attach_db(&db, || {
        let analysis = BodyAnalysis::new(&db, DefWithBody::from(func(&db, "takes")));
        let (root, file) = analysis.tree();
        let (call, ptr) = find::<ast::MethodCallExpr>(root, file, "w.show()");
        let method = analysis.resolve_method_call(ptr).expect("`show` resolves");
        assert_eq!(method.name(&db).as_str(), "show");

        let id = analysis
            .source_map()
            .store
            .node_expr_view(InFile::new(file, view::Expr::MethodCallExpr(call)))
            .and_then(|it| it.as_expr())
            .expect("the call is of the body");
        let (callee, _) = analysis.callee(id).expect("the call has a callee");
        assert!(matches!(callee.lookup(&db).container, ItemContainerId::ImplId(_)));
        assert_eq!(Function::from(callee), method);
    });
}

#[test]
fn paths_and_names_resolve_in_the_scope_of_their_block() {
    let db = fixture::db(BODY).into_db();
    crate::attach_db(&db, || {
        let analysis = BodyAnalysis::new(&db, DefWithBody::from(func(&db, "takes")));
        let (root, file) = analysis.tree();

        let (_, wrap) = find::<ast::Path>(root, file, "Wrap");
        match analysis.resolve_path(wrap) {
            Some(PathResolution::Def(ModuleDef::Adt(it))) => {
                assert_eq!(it.name(&db).as_str(), "Wrap")
            }
            it => panic!("`Wrap` is {it:?}"),
        }
        let (_, n) = find::<ast::Path>(root, file, "n");
        assert!(matches!(analysis.resolve_path(n), Some(PathResolution::Local(_))));

        let (sum, _) = find::<ast::Expr>(root, file, "a + b + c");
        let at = InFile::new(file, view::ptr(sum.syntax()));
        let inner = Name::new_symbol_root(intern::Symbol::intern("Inner"));
        match analysis.resolve_name(at, &inner) {
            Some(PathResolution::Def(ModuleDef::Adt(it))) => {
                assert_eq!(it.name(&db).as_str(), "Inner")
            }
            it => panic!("`Inner` is {it:?}"),
        }
        let w = Name::new_symbol_root(intern::Symbol::intern("w"));
        assert!(matches!(analysis.resolve_name(at, &w), Some(PathResolution::Local(_))));
    });
}

#[test]
fn types_resolve_in_the_body_the_signature_and_a_nested_item() {
    let db = fixture::db(BODY).into_db();
    crate::attach_db(&db, || {
        let analysis = BodyAnalysis::new(&db, DefWithBody::from(func(&db, "takes")));
        let target = target(&db, "takes");
        let (root, file) = analysis.tree();
        let plains: Vec<_> = root
            .descendants()
            .filter_map(view::Type::cast)
            .filter(|it| it.syntax().text() == "Plain")
            .collect();
        // The parameter's, `&Plain`'s, the nested struct's field's and the `let`'s.
        assert_eq!(plains.len(), 4);
        for ty in &plains {
            let found = analysis.resolve_type(InFile::new(file, AstPtr::<ast::Type>::of(*ty)));
            let parent = ty.syntax().parent();
            let found = found.unwrap_or_else(|| panic!("no type for `{ty}` in {parent:?}"));
            assert_eq!(show(&db, &found, target), "Plain");
        }
    });
}

#[test]
fn a_macro_call_resolves_and_its_expressions_are_where_the_call_is() {
    let db = fixture::db(BODY).into_db();
    crate::attach_db(&db, || {
        let analysis = BodyAnalysis::new(&db, DefWithBody::from(func(&db, "takes")));
        let (root, file) = analysis.tree();
        let (call, ptr) = find::<ast::MacroCall>(root, file, "twice!(n)");
        let mac = analysis.resolve_macro_call(ptr).expect("`twice` resolves");
        assert_eq!(mac.name(&db).as_str(), "twice");
        let expansion = analysis.expansion(ptr).expect("the call is of the body");

        let in_expansion: Vec<ExprId> = analysis
            .body()
            .store
            .expr_ids()
            .filter(|&id| {
                let syntax = analysis.source_map().store.expr_syntax(id);
                syntax.is_ok_and(|it| it.file_id == HirFileId::from(expansion))
            })
            .collect();
        assert!(!in_expansion.is_empty());
        for id in in_expansion {
            assert_eq!(analysis.expr_range(id), Some(call.syntax().text_range()));
        }
    });
}

#[test]
fn a_moved_local_is_consumed_and_a_read_one_is_not() {
    let db = fixture::db(BODY).into_db();
    crate::attach_db(&db, || {
        let analysis = BodyAnalysis::new(&db, DefWithBody::from(func(&db, "takes")));
        assert!(analysis.consumes(local(&analysis, "v")));
        assert!(!analysis.consumes(local(&analysis, "r")));
        assert!(!analysis.consumes(local(&analysis, "n")));
        assert!(!analysis.consumes(local(&analysis, "w")));

        let captures = BodyAnalysis::new(&db, DefWithBody::from(func(&db, "captures")));
        let p = local(&captures, "p");
        assert!(captures.consumes(p), "a `move` closure moves what it captures");
        assert!(!captures.consumes(local(&captures, "q")), "a closure borrowing `q` moves nothing");
        assert!(!captures.consumes(local(&analysis, "v")), "a local of another body");
    });
}

#[test]
fn a_body_a_macro_writes_is_read_off_its_expansion() {
    let db = fixture::db(BODY).into_db();
    crate::attach_db(&db, || {
        let analysis = BodyAnalysis::new(&db, DefWithBody::from(func(&db, "made")));
        let target = target(&db, "made");
        let (root, file) = analysis.tree();
        assert!(matches!(file, HirFileId::MacroFile(_)), "{file:?}");
        assert_eq!(root.kind(), SyntaxKind::MACRO_ITEMS);
        let p = local(&analysis, "p");
        assert!(analysis.consumes(p));
        let (_, ty) = find::<ast::Type>(root, file, "Plain");
        let found = analysis.resolve_type(ty).expect("the parameter's type");
        assert_eq!(show(&db, &found, target), "Plain");
    });
}
