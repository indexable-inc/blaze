//! The crate's `use` and `extern crate` items from its def maps against the syntax walk they
//! replace: the syntax-driven enumeration (each `USE` and `EXTERN_CRATE` node of the walk's
//! trees, placed by `vsema`, its trees read from the node) is kept here as the reference model.

use std::alloc::Global;

use blaze_hash::HashMap;
use hir::InFile;
use hir_def::{
    ExternCrateId, Lookup as _, ModuleId, UseId,
    item_scope::{GlobId, ImportId},
    src::use_tree_sources_view,
};
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::{AsName as _, Name},
};
use intern::sym;
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasVisibility as _},
};

use super::{
    Places,
    used::{Binding, TreeKind, Uses},
};
use crate::{Files, cx::Cx};

/// A `use` item as both sides give it: its id, module, `pub`, and each tree's parent, kind,
/// binding, `as _`, own path (as `resolve_use` reads it: `*` and `{..}` with none are a plain
/// empty path) and whether it is an empty list.
type UseRow = (UseId, ModuleId, bool, Vec<TreeRow>);

/// A tree of a [`UseRow`].
type TreeRow = (Option<u32>, TreeKind, Option<Binding>, bool, Option<ModPath>, bool);

/// An `extern crate` item as both sides give it: id, module, the name it binds, renamed,
/// `pub`, the crate it loads, `#[macro_use]`.
type CrateRow = (ExternCrateId, ModuleId, Name, bool, bool, Option<hir::Crate>, bool);

/// The rows [`Uses::of`] reads from the def maps, in its order.
fn from_def_maps(uses: &Uses<'_>) -> (Vec<UseRow>, Vec<CrateRow>) {
    let plain = ModPath::from_kind(PathKind::Plain);
    let items = uses
        .items
        .iter()
        .map(|item| {
            let trees = item
                .trees
                .iter()
                .map(|it| {
                    let own = Some(it.path.unwrap_or(plain));
                    (it.parent, it.kind, it.binding, it.underscore, own, it.empty)
                })
                .collect(); // alloc: a test's rows
            (item.id, item.module, item.public, trees)
        })
        .collect(); // alloc: a test's rows
    let crates = uses
        .crates
        .iter()
        .map(|it| (it.id, it.module, it.name, it.renames, it.public, it.krate, it.macro_use))
        .collect(); // alloc: a test's rows
    (items, crates)
}

/// The reference model: the rows as the deleted syntax-driven first loop of [`Uses::of`] found
/// them, sorted as it sorted them.
fn from_syntax(cx: &Cx<'_, '_>, places: &Places) -> (Vec<UseRow>, Vec<CrateRow>) {
    let db = cx.db;
    let walk = cx.walk();
    // alloc: a test's rows and index
    let mut items: Vec<(InFile<syntax::SyntaxNodePtr>, UseRow)> = Vec::new();
    let mut crates: Vec<(InFile<syntax::SyntaxNodePtr>, CrateRow)> = Vec::new();
    let mut use_ids: HashMap<(hir::HirFileId, TextSize), UseId, Global> = HashMap::new_in(Global);
    for tree in &walk.trees {
        for node in tree.of_kind(|it| matches!(it, SyntaxKind::USE | SyntaxKind::EXTERN_CRATE)) {
            let ptr = InFile::new(tree.file, ast::ptr(node));
            if let Some(node) = ast::Use::cast(node) {
                let Some(scope) = cx.vsema.scope(node.syntax()) else { continue };
                let module = ModuleId::from(scope.module());
                for id in module.def_map(db).module(db, module).scope(db).use_decls() {
                    let loc = id.lookup(db);
                    use_ids.insert((loc.id.file_id, loc.id.to_ptr(db).text_range().start()), id);
                }
                let Some(&id) = use_ids.get(&(tree.file, node.syntax().text_range().start()))
                else {
                    continue;
                };
                items.push((ptr, use_row(cx, id, module, node)));
            } else if let Some(node) = ast::ExternCrate::cast(node)
                && let Some(row) = crate_row(cx, node)
            {
                crates.push((ptr, row));
            }
        }
    }
    items.sort_by_cached_key(|(at, _)| places.place(cx, *at));
    crates.sort_by_cached_key(|(at, _)| places.place(cx, *at));
    (
        items.into_iter().map(|(_, it)| it).collect(), // alloc: a test's rows
        crates.into_iter().map(|(_, it)| it).collect(), // alloc: a test's rows
    )
}

/// The deleted `Uses::use_item` and `resolve_use`'s own path of each tree, as a row.
fn use_row(cx: &Cx<'_, '_>, id: UseId, module: ModuleId, node: ast::Use<'_>) -> UseRow {
    let db = cx.db;
    let sources = use_tree_sources_view(db, id);
    let span_map = sources.file_id.span_map(db);
    let public = node.visibility().is_some_and(|it| matches!(it.kind(), ast::VisibilityKind::Pub));
    let mut trees = Vec::new(); // alloc: a test's rows
    let mut open: Vec<(TextRange, u32)> = Vec::new(); // alloc: a test's stack
    for tree in node.syntax().descendants().filter_map(ast::UseTree::cast) {
        let range = tree.syntax().text_range();
        while open.last().is_some_and(|(it, _)| !it.contains_range(range)) {
            open.pop();
        }
        let parent = open.last().map(|&(_, it)| it);
        open.push((range, trees.len() as u32));
        let idx = sources
            .value
            .iter()
            .find(|(_, it)| it.syntax().text_range() == range)
            .map(|(idx, _)| idx);
        let kind = if tree.use_tree_list().is_some() {
            TreeKind::List
        } else if tree.star_token().is_some() {
            TreeKind::Glob
        } else {
            TreeKind::Simple
        };
        let binding = match (kind, idx) {
            (TreeKind::List, _) | (_, None) => None,
            (TreeKind::Glob, Some(idx)) => Some(Binding::Glob(GlobId { use_: id, idx })),
            (TreeKind::Simple, Some(idx)) => Some(Binding::Import(ImportId { use_: id, idx })),
        };
        let underscore = tree.rename().is_some_and(|it| it.underscore_token().is_some());
        let own = match tree.path() {
            Some(path) => ModPath::from_src_view(db, path, &mut |range| {
                span_map.span_for_range(range).ctx
            }),
            None => Some(ModPath::from_kind(if tree.coloncolon_token().is_some() {
                PathKind::Abs
            } else {
                PathKind::Plain
            })),
        };
        let empty = kind == TreeKind::List
            && tree.use_tree_list().is_none_or(|it| it.use_trees().next().is_none());
        trees.push((parent, kind, binding, underscore, own, empty));
    }
    (id, module, public, trees)
}

/// The deleted `Uses::extern_crate`, as a row.
fn crate_row<'db>(cx: &Cx<'_, 'db>, node: ast::ExternCrate<'db>) -> Option<CrateRow> {
    let db = cx.db;
    let decl = cx.vsema.to_def(node)?;
    let id = ExternCrateId::from(decl);
    let module = id.lookup(db).container;
    let name = match node.rename() {
        Some(rename) if rename.underscore_token().is_some() => Name::new_symbol_root(sym::underscore),
        Some(rename) => ast::HasName::name(&rename)?.as_name(),
        None => node.name_ref()?.as_name(),
    };
    let owner = ast::AnyHasAttrs::new(node);
    let macro_use = cx.walk().attrs(owner).any(|(meta, _)| {
        let path = match &meta {
            ast::Meta::PathMeta(it) => it.path(),
            ast::Meta::TokenTreeMeta(it) => it.path(),
            _ => None,
        };
        path.and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "macro_use")
    });
    let public = node.visibility().is_some_and(|it| matches!(it.kind(), ast::VisibilityKind::Pub));
    let renames = node.rename().is_some();
    Some((id, module, name, renames, public, decl.resolved_crate(db), macro_use))
}

/// What the test plants in the def maps' side, to show the comparison catches it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Plant {
    None,
    /// The `use` items with a glob tree left out of the def maps' rows.
    GlobLeftOut,
}

/// Both sides' rows of the crate of `source` (one file, edition `edition`).
fn rows(source: &str, edition: &str, plant: Plant) -> [(Vec<UseRow>, Vec<CrateRow>); 2] {
    let fixture = format!("//- /main.rs crate:main edition:{edition}\n{source}");
    let (db, names, _) = crate::tests::fixture_db(&fixture, "");
    blaze_query::db::attach(&db, || {
        let krate = hir::check::workspace_crates(&db)[0];
        let frame = crate::tests::frame();
        let cx = Cx::new(&db, krate, Files::new(&names, ""), &frame);
        let places = Places::of(&cx);
        let uses = Uses::of(&cx, &places);
        let mut ours = from_def_maps(&uses);
        if plant == Plant::GlobLeftOut {
            ours.0.retain(|(_, _, _, trees)| trees.iter().all(|it| it.1 != TreeKind::Glob));
        }
        [ours, from_syntax(&cx, &places)]
    })
}

/// Every fixture under `src/tests/lints/names/*/` (the files of rustc's UI tests the names
/// lints are checked against, their auxiliary crates included), with its path.
pub(super) fn names_fixtures() -> Vec<(String, String)> {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tests/lints/names");
    // alloc: a test's list of fixtures and its walk
    let mut found = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(root.into(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("the names fixtures' directory reads") {
            let path = entry.expect("a names fixture's entry reads").path();
            if path.is_dir() {
                stack.push((path, depth + 1));
            } else if depth > 0 && path.extension().is_some_and(|it| it == "rs") {
                let text = std::fs::read_to_string(&path).expect("a names fixture reads");
                found.push((path.display().to_string(), text));
            }
        }
    }
    found.sort();
    found
}

/// A glob import the control's plant leaves out.
const GLOB: &str = r#"
mod a { pub struct S; pub mod b { pub struct T; } }
use a::*;
use a::{b::{self, T}, S as _};
extern crate core as k;
fn main() { let _ = S; let _: T; }
"#;

/// The refinement: on every names fixture, in editions 2015 and 2021, the `use` items and
/// `extern crate` items `Uses::of` reads from the def maps and item trees are exactly those the
/// syntax walk found, in the same order, each tree with the same parent, kind, binding, `as _`,
/// own path and emptiness. Control: [`Plant::GlobLeftOut`] is caught on a fixture with a glob.
#[test]
fn the_def_maps_items_are_the_syntax_walks() {
    let fixtures = names_fixtures();
    assert!(fixtures.len() >= 40, "{} names fixtures", fixtures.len());
    let mut items = 0;
    for (path, source) in &fixtures {
        for edition in ["2015", "2021"] {
            let [ours, reference] = rows(source, edition, Plant::None);
            assert_eq!(ours, reference, "{path} in edition {edition}");
            items += ours.0.len() + ours.1.len();
        }
    }
    assert!(items >= 60, "{items} items over the names fixtures");
    let [ours, reference] = rows(GLOB, "2021", Plant::None);
    assert_eq!(ours, reference, "the control's fixture holds unplanted");
    let [ours, reference] = rows(GLOB, "2021", Plant::GlobLeftOut);
    assert_ne!(ours, reference, "a glob `use` left out of the def maps' rows is caught");
}
