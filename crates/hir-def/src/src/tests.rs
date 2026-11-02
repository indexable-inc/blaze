//! The views of this module on a fixture rich in items, children and use trees: each item's
//! source view against its handle, and each child and use tree's node as a stored expectation.

use std::fmt::Write as _;

use expect_test::expect_file;
use la_arena::{Idx, RawIdx};
use syntax::{SyntaxKind, TextRange};
use hir_expand::fixture;

use super::*;
use crate::{
    AdtId, EnumVariantId, GenericDefId, ModuleDefId,
    nameres::crate_def_map,
    signatures::EnumVariants,
    test_db::{TestDB, TestDBExt},
};

const FIXTURE: &str = r#"
//- /main.rs crate:main cfg:on
use std::{fmt::{self, Display}, collections::*, io as _};
use self::inner as renamed;
mod inner {
    pub struct Unit;
    pub fn nested<'n, N: Copy>(_: &'n N) {}
}
struct Tuple<'a, T: Copy, const N: usize>(#[cfg(on)] u8, #[cfg(off)] u16, &'a T);
struct Record<T> {
    a: T,
    #[cfg(off)]
    gone: u8,
    b: [T; 2],
}
union U {
    x: u32,
    #[cfg(not(on))]
    y: f32,
    z: i64,
}
enum E<'x> {
    A,
    B(u8, #[cfg(off)] u8, &'x str),
    C { f: u8, #[cfg(on)] g: u16 },
}
trait Tr<'a, X, const C: bool> {
    fn m<'b, Y>(&self);
}
fn f<'a, 'b: 'a, T, const K: u8>() {}
impl<'q, Q: Copy> Tr<'q, Q, true> for Tuple<'q, Q, 1> {
    fn m<'b, Y>(&self) {}
}
"#;

fn of_view(node: view::Node<'_>) -> (SyntaxKind, TextRange) {
    (node.kind(), node.text_range())
}

fn of_handle(node: &syntax::SyntaxNode) -> (SyntaxKind, TextRange) {
    (node.kind(), node.text_range())
}

fn either_view<'t, L: view::AstNode<'t>, R: view::AstNode<'t>>(
    it: &Either<L, R>,
) -> (SyntaxKind, TextRange) {
    of_view(view::AstNode::syntax(it))
}

/// Every item, generic owner and variant of the fixture's crate, and every import.
struct Defs {
    sources: Vec<(SyntaxKind, TextRange, SyntaxKind, TextRange)>,
    generics: Vec<GenericDefId>,
    variants: Vec<VariantId>,
    uses: Vec<UseId>,
}

fn defs(db: &TestDB) -> Defs {
    fn source<L: HasSource>(db: &Db, loc: &L) -> (SyntaxKind, TextRange, SyntaxKind, TextRange)
    where
        L::Value: Typed,
    {
        let handle = loc.source(db);
        let view = loc.source_view(db);
        assert_eq!(handle.file_id, view.file_id);
        let (vk, vr) = of_view(view::AstNode::syntax(&view.value));
        let (hk, hr) = of_handle(handle.value.syntax());
        (vk, vr, hk, hr)
    }

    let mut defs =
        Defs { sources: Vec::new(), generics: Vec::new(), variants: Vec::new(), uses: Vec::new() };
    let def_map = crate_def_map(db, db.fetch_test_crate());
    for (_, module) in def_map.modules(db) {
        let scope = module.scope(db);
        defs.uses.extend(scope.use_decls());
        for it in scope.use_decls() {
            defs.sources.push(source(db, it.lookup(db)));
        }
        for it in scope.impls() {
            defs.sources.push(source(db, it.lookup(db)));
            defs.generics.push(it.into());
        }
        for decl in scope.declarations() {
            match decl {
                ModuleDefId::FunctionId(it) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                }
                ModuleDefId::AdtId(AdtId::StructId(it)) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                    defs.variants.push(VariantId::StructId(it));
                }
                ModuleDefId::AdtId(AdtId::UnionId(it)) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                    defs.variants.push(VariantId::UnionId(it));
                }
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                    for &(variant, _) in EnumVariants::of(db, it).0.variants.values() {
                        let variant: EnumVariantId = variant;
                        defs.sources.push(source(db, variant.lookup(db)));
                        defs.variants.push(VariantId::EnumVariantId(variant));
                    }
                }
                ModuleDefId::TraitId(it) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                }
                _ => {}
            }
        }
    }
    defs
}

#[test]
fn source_view_is_source() {
    let db = fixture::db(FIXTURE).into_db();
    let defs = defs(&db);
    assert!(defs.sources.len() >= 14, "the fixture's items: {}", defs.sources.len());
    for (vk, vr, hk, hr) in defs.sources {
        assert_eq!((vk, vr), (hk, hr));
    }
}

/// Each child's node, by owner (numbered in the order [`defs`] walks them) and index, up to and
/// including the first index past the last child, which has none.
#[test]
fn child_source_view() {
    let db = fixture::db(FIXTURE).into_db();
    let defs = defs(&db);
    let mut out = String::new();
    let mut seen = 0;
    for (owner, variant) in defs.variants.iter().enumerate() {
        for raw in 0.. {
            let id = LocalFieldId::from_raw(RawIdx::from(raw));
            let view = variant.child_source_view(&db, id);
            let node = view.value.as_ref().map(either_view);
            writeln!(out, "variant {owner} field {raw}: {:?} {node:?}", view.file_id).unwrap();
            if node.is_none() {
                break;
            }
            seen += 1;
        }
    }
    for (owner, def) in defs.generics.iter().enumerate() {
        for raw in 0.. {
            let id = LocalTypeOrConstParamId::from_raw(RawIdx::from(raw));
            let view = def.child_source_view(&db, id);
            let node = view.value.as_ref().map(either_view);
            writeln!(out, "generics {owner} type or const {raw}: {:?} {node:?}", view.file_id)
                .unwrap();
            if node.is_none() {
                break;
            }
            seen += 1;
        }
        for raw in 0.. {
            let id = LocalLifetimeParamId::from_raw(RawIdx::from(raw));
            let view = def.child_source_view(&db, id);
            let node = view.value.map(|it| of_view(view::AstNode::syntax(&it)));
            writeln!(out, "generics {owner} lifetime {raw}: {:?} {node:?}", view.file_id).unwrap();
            if node.is_none() {
                break;
            }
            seen += 1;
        }
    }
    assert_eq!(
        seen, 27,
        "the fixture's children: 10 fields, 10 type or const parameters (with `Tr`'s `Self`) and \
         7 lifetimes"
    );
    expect_file!["child_source_view.txt"].assert_eq(&out);
}

/// Each use tree of each import (numbered in the order [`defs`] walks them): the arena of
/// [`use_trees_view`], [`use_tree_to_view`] and [`use_tree_sources_view`] give the same node.
#[test]
fn use_tree_views() {
    let db = fixture::db(FIXTURE).into_db();
    let mut out = String::new();
    let mut seen = 0;
    for (owner, use_) in defs(&db).uses.into_iter().enumerate() {
        let id = use_.lookup(&db).id;
        let trees = use_trees_view(&db, id);
        let views = use_tree_sources_view(&db, use_);
        assert_eq!(views.value.iter().count(), trees.len());
        for (idx, tree) in trees.iter() {
            let index: Idx<ast::UseTree> = Idx::from_raw(idx.into_raw());
            let node = of_view(view::AstNode::syntax(tree));
            let view = use_tree_to_view(&db, id, index);
            assert_eq!(of_view(view::AstNode::syntax(&view)), node);
            let in_views = views.value.get(index).map(|&it| of_view(view::AstNode::syntax(&it)));
            assert_eq!(in_views, Some(node));
            writeln!(out, "use {owner} tree {}: {:?} {node:?}", u32::from(idx.into_raw()), views.file_id)
                .unwrap();
            seen += 1;
        }
    }
    assert_eq!(seen, 7, "the fixture's use trees");
    expect_file!["use_tree_views.txt"].assert_eq(&out);
}
