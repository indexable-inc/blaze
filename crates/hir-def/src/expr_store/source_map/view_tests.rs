//! The view twins of the source map's lookups against their handle versions: for every node of
//! each kind under every body of a fixture, the same id.

use hir_expand::InFile;
use syntax::{AstPtr, SyntaxNode, ast, view};
use hir_expand::fixture;

use crate::{
    DefWithBodyId, Lookup, ModuleDefId,
    expr_store::Body,
    nameres::crate_def_map,
    src::HasSource,
    test_db::{TestDB, TestDBExt},
};

const FIXTURE: &str = r#"
//- minicore: fmt, range
macro_rules! m {
    ($e:expr) => { $e };
}
struct S { x: u8, y: u8 }
fn f(a: u8, (b, c): (u8, u8), S { x, .. }: S) -> u8 {
    let arr: [u8; 2] = [a, b];
    'outer: loop {
        if c > 0 {
            break 'outer;
        }
    }
    let _ = format_args!("{a} {}", c);
    let n = m!(arr[0]);
    let _r: core::ops::Range<u8> = 0..n;
    match a {
        0 | 1 => b,
        k @ 2.. => k + x,
        _ => { let t: (u8, &u8) = (c, &c); t.0 }
    }
}
const K: u8 = m!(1 + 2);
static V: [u8; 1] = [K];
"#;

/// The bodies of the fixture with the file they are written in.
fn bodies(db: &TestDB) -> Vec<(DefWithBodyId, hir_expand::HirFileId)> {
    let def_map = crate_def_map(db, db.fetch_test_crate());
    let mut bodies = Vec::new();
    for (_, module) in def_map.modules(db) {
        for decl in module.scope(db).declarations() {
            let it = match decl {
                ModuleDefId::FunctionId(it) => (it.into(), it.lookup(db).ast_ptr(db).file_id),
                ModuleDefId::ConstId(it) => (it.into(), it.lookup(db).ast_ptr(db).file_id),
                ModuleDefId::StaticId(it) => (it.into(), it.lookup(db).ast_ptr(db).file_id),
                _ => continue,
            };
            bodies.push(it);
        }
    }
    bodies
}

/// The handle of the node a view is.
fn handle<N: syntax::AstNode + view::Typed>(node: N::Of<'_>, root: &SyntaxNode) -> N {
    AstPtr::<N>::of(node).to_node(root)
}

#[test]
fn every_lookup_view_is_its_handle() {
    let db = fixture::db(FIXTURE).into_db();
    let mut found = [0usize; 7];
    let bodies = bodies(&db);
    assert_eq!(bodies.len(), 3);
    for (def, file_id) in bodies {
        let (_, source_map) = Body::with_source_map(&db, def);
        // The body's file and those of the macro calls in it: a format string is in its
        // `format_args!`'s expansion.
        let expansions = source_map.macro_calls().map(|(_, call)| hir_expand::HirFileId::from(call));
        for file_id in std::iter::once(file_id).chain(expansions) {
            let handles = file_id.parse_or_expand(&db);
            let root = file_id.root(&db);
            for node in root.descendants() {
                if let Some(it) = <view::Expr<'_> as view::AstNode<'_>>::cast(node) {
                    let h = handle::<ast::Expr>(it, &handles);
                    let id = source_map.node_expr_view(InFile::new(file_id, it));
                    assert_eq!(id, source_map.node_expr(InFile::new(file_id, &h)), "{def:?} {node:?}");
                    found[0] += usize::from(id.is_some());
                }
                if let Some(it) = <view::Pat<'_> as view::AstNode<'_>>::cast(node) {
                    let h = handle::<ast::Pat>(it, &handles);
                    let id = source_map.node_pat_view(InFile::new(file_id, it));
                    assert_eq!(id, source_map.node_pat(InFile::new(file_id, &h)), "{def:?} {node:?}");
                    found[1] += usize::from(id.is_some());
                }
                if let Some(it) = <view::Type<'_> as view::AstNode<'_>>::cast(node) {
                    let h = handle::<ast::Type>(it, &handles);
                    let id = source_map.node_type_view(InFile::new(file_id, it));
                    assert_eq!(id, source_map.node_type(InFile::new(file_id, &h)), "{def:?} {node:?}");
                    found[2] += usize::from(id.is_some());
                }
                if let Some(it) = <view::Label<'_> as view::AstNode<'_>>::cast(node) {
                    let h = handle::<ast::Label>(it, &handles);
                    let id = source_map.node_label_view(InFile::new(file_id, it));
                    assert_eq!(id, source_map.node_label(InFile::new(file_id, &h)), "{def:?} {node:?}");
                    found[3] += usize::from(id.is_some());
                }
                if let Some(it) = <view::MacroCall<'_> as view::AstNode<'_>>::cast(node) {
                    let h = handle::<ast::MacroCall>(it, &handles);
                    let id = source_map.node_macro_file_view(InFile::new(file_id, it));
                    assert_eq!(id, source_map.node_macro_file(InFile::new(file_id, &h)), "{def:?} {node:?}");
                    assert_eq!(source_map.expansion_view(InFile::new(file_id, it)), source_map.expansion(InFile::new(file_id, &h)));
                    found[4] += usize::from(id.is_some());
                }
                if let Some(it) = <view::MacroExpr<'_> as view::AstNode<'_>>::cast(node) {
                    let h = handle::<ast::MacroExpr>(it, &handles);
                    let id = source_map.macro_expansion_expr_view(InFile::new(file_id, it));
                    assert_eq!(id, source_map.macro_expansion_expr(InFile::new(file_id, &h)), "{def:?} {node:?}");
                    found[5] += usize::from(id.is_some());
                }
                if let Some(it) = <view::FormatArgsExpr<'_> as view::AstNode<'_>>::cast(node) {
                    let h = handle::<ast::FormatArgsExpr>(it, &handles);
                    let view = source_map.implicit_format_args_view(InFile::new(file_id, it));
                    assert_eq!(view, source_map.implicit_format_args(InFile::new(file_id, &h)), "{def:?} {node:?}");
                    found[6] += usize::from(view.is_some());
                }
                if let Some(it) = <view::AsmExpr<'_> as view::AstNode<'_>>::cast(node) {
                    let h = handle::<ast::AsmExpr>(it, &handles);
                    let view = source_map.asm_template_args_view(InFile::new(file_id, it));
                    assert_eq!(view, source_map.asm_template_args(InFile::new(file_id, &h)), "{def:?} {node:?}");
                }
            }
        }
    }
    // Every kind the fixture writes is found: expressions, patterns, types, the label, the
    // macro calls, and the format string.
    assert!(found[0] >= 20 && found[1] >= 8 && found[2] >= 4, "{found:?}");
    assert!(found[3] >= 1 && found[4] >= 2 && found[6] >= 1, "{found:?}");
}
