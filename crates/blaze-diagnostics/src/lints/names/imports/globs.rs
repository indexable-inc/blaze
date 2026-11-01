//! What rustc lints of the names imports bring in once every import is resolved: a glob
//! import more visible than anything it imports (`unused_imports`), a `pub use` of a private
//! `extern crate` item (`pub_use_of_private_extern_crate`), two public globs that re-export
//! different items under one name (`ambiguous_glob_reexports`), and a private item that
//! shadows a name a public glob re-exports (`hidden_glob_reexports`).
//!
//! A glob from a module with unnamed trait imports (`use Trait as _`) is not checked for its
//! visibility: hir-def does not keep how visible those are.
//!
//! Follows rustc's `compiler/rustc_resolve/src/imports.rs` (`finalize_import`'s glob
//! visibility check and `report_cannot_reexport`, `pub_use_of_private_extern_crate_hack`,
//! `lint_reexports`, `select_glob_decl`'s glob-glob ambiguity, `new_import_decl`'s
//! `max_vis`), `compiler/rustc_middle/src/ty/mod.rs` (`Visibility::to_string`) and
//! `compiler/rustc_resolve/src/effective_visibilities.rs` (MIT or Apache-2.0).

use hir::{InFile, ModuleDef, view::HasSourceView as _};
use hir_def::{MacroId, ModuleDefId, ModuleId, visibility::Visibility};
use hir_expand::{mod_path::ModPath, name::Name};
use blaze_hash::FixedState;
use smallvec::SmallVec;
use syntax::view::{self as ast, AstNode as _, HasVisibility, Node};

use super::used::{Binding, Entry, Ns, Tree, TreeKind, UseItem, Uses, entry};
use crate::{
    cx::{Cx, item_range_view},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// The lints of this module, in the order rustc buffers them: each import's as it is
/// finalized, then each module's names'.
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, uses: &Uses<'_, 'db>) -> Vec<Found> {
    // alloc: holds only diagnostics that fire; empty, it allocates nothing
    let mut found = Vec::new();
    for item in &uses.items {
        for tree in &item.trees {
            match tree.kind {
                TreeKind::Glob => found.extend(glob_visibility(cx, uses, item, tree)),
                TreeKind::Simple => found.extend(private_extern_crate(cx, uses, item, tree)),
                TreeKind::List => {}
            }
        }
    }
    let mut seen: hashbrown::HashSet<ModuleId, FixedState, _> =
        hashbrown::HashSet::with_capacity_and_hasher_in(uses.globs.len(), FixedState, cx.frame());
    for module in uses.items.iter().map(|it| it.module) {
        if uses.globs.contains_key(&module) && seen.insert(module) {
            found.extend(reexports(cx, uses, module));
        }
    }
    found
}

/// rustc's check that a glob import re-exports something as visibly as it is declared: its
/// visibility greater than `max_vis`, the greatest of those of what it imports.
fn glob_visibility(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, item: &UseItem<'_>, tree: &Tree<'_>) -> Option<Found> {
    let db = cx.db;
    let Some(ModuleDefId::ModuleId(from)) = tree.source else { return None };
    // A private import is as visible as anything it can import, so `max_vis` never falls below it.
    if from == item.module || item.node.value.visibility().is_none() {
        return None;
    }
    let home = nearest_module(cx, item.module);
    let import_vis = item_visibility(cx, uses, item)?;
    let scope = from.def_map(db).module(db, from).scope(db);
    let mut named: Vec<hir_def::TraitId, _> = Vec::new_in(cx.frame());
    named.extend(scope.types().filter_map(|(_, it)| match it.def {
        ModuleDefId::TraitId(it) => Some(it),
        _ => None,
    }));
    if from.def_map(db).module(db, from).traits_in_scope(db).any(|it| !named.contains(&it)) {
        return None;
    }
    let mut max: Option<Vis> = None;
    for (_, per) in scope.entries() {
        for ns in Ns::ALL {
            let Some(found) = entry(&per, ns) else { continue };
            if !found.vis.is_visible_from(db, item.module) {
                continue;
            }
            if let ModuleDefId::MacroId(MacroId::MacroRulesId(_)) = found.def
                && found.vis != Visibility::Public
            {
                continue;
            }
            let vis = Vis::min(cx, Vis::of(cx, found.vis), import_vis);
            if vis == import_vis || max.is_none_or(|max| Vis::greater_than(cx, vis, max)) {
                max = Some(vis);
            }
        }
    }
    let max = max?;
    if !Vis::greater_than(cx, import_vis, max) {
        return None;
    }
    let span = cx.span(InFile::new(item.node.file_id, tree.node.syntax().text_range()));
    // alloc: the message of a lint that fires
    let message = format!(
        "glob import doesn't reexport anything with visibility `{}` because no imported item is public enough",
        import_vis.to_string(cx, home)
    );
    // clone: the lint fires; its span is both the primary span and the note's
    let mut diag = Diag::lint("unused_imports", message, span.clone());
    // alloc: the note of a lint that fires
    diag.span_note(span, format!("the most public imported item is `{}`", max.to_string(cx, home)));
    diag.help("reduce the glob import's visibility or increase visibility of imported items");
    diag.fact(cx.resolves_fact(from.into(), "*"), "what the glob import brings in");
    let at = InFile::new(item.node.file_id, ast::ptr(tree.node.syntax()));
    Some(Found::lint(diag, Phase::Resolve, at))
}

/// `pub_use_of_private_extern_crate`: a `pub` single import of nothing but an `extern crate`
/// item less visible than it.
fn private_extern_crate(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, item: &UseItem<'_>, tree: &Tree<'_>) -> Option<Found> {
    if !item.public || tree.self_import || tree.binding.is_none() {
        return None;
    }
    let Some(Entry { import: Some(Binding::ExternCrate(id)), .. }) = entry(&tree.imported, Ns::Types) else {
        return None;
    };
    if entry(&tree.imported, Ns::Values).is_some() || entry(&tree.imported, Ns::Macros).is_some() {
        return None;
    }
    let krate = uses.extern_crate_of(id)?;
    if krate.public {
        return None;
    }
    let path = tree.node.path()?;
    // alloc: the lint fires from here on; the name goes into its message and fact
    let ident = path.segment()?.name_ref()?.text().to_string();
    let span = cx.span(InFile::new(item.node.file_id, tree.node.syntax().text_range()));
    let mut diag = Diag::lint(
        "pub_use_of_private_extern_crate",
        // alloc: the message of a lint that fires
        format!("extern crate `{ident}` is private and cannot be re-exported"),
        span,
    );
    let start = item_range_view(krate.node.value.syntax());
    let sugg = cx.span(InFile::new(krate.node.file_id, start)).shrink_to_lo();
    diag.suggestion_verbose(
        sugg,
        "consider making the `extern crate` item publicly accessible",
        "pub ",
        Applicability::MaybeIncorrect,
    );
    // alloc: the fact of a lint that fires
    diag.fact(cx.resolves_fact(item.module.into(), &ident), format!("what `{ident}` is imported from"));
    let at = InFile::new(item.node.file_id, ast::ptr(tree.node.syntax()));
    Some(Found::lint(diag, Phase::Resolve, at))
}

/// `ambiguous_glob_reexports` and `hidden_glob_reexports` of the names of `module`.
fn reexports<'db>(cx: &Cx<'_, 'db>, uses: &Uses<'_, 'db>, module: ModuleId) -> Vec<Found> {
    let db = cx.db;
    // alloc: holds only diagnostics that fire; empty, it allocates nothing
    let mut found = Vec::new();
    let globs: SmallVec<[(&UseItem<'db>, &Tree<'db>, ModuleId); 2]> = uses
        .globs_of(module)
        .filter_map(|(item, tree)| match tree.source {
            Some(ModuleDefId::ModuleId(from)) if from != module => Some((item, tree, from)),
            _ => None,
        })
        .collect(); // alloc: inline for the two globs a module has at most, almost always
    // Both lints need a `pub` glob: the first of an ambiguity, or the one a private item hides.
    if !globs.iter().any(|(item, _, _)| item.public) {
        return found;
    }
    let here = module.def_map(db).module(db, module).scope(db);
    let frame = cx.frame();
    let mut names: Vec<Name, _> = Vec::new_in(frame);
    let mut seen: hashbrown::HashSet<Name, FixedState, _> = hashbrown::HashSet::with_hasher_in(FixedState, frame);
    for (_, _, from) in &globs {
        for (name, _) in from.def_map(db).module(db, *from).scope(db).entries() {
            if seen.insert(*name) {
                names.push(*name);
            }
        }
    }
    for name in &names {
        let current = here.get(name);
        for ns in Ns::ALL {
            let Some(it) = entry(&current, ns) else { continue };
            let imported: SmallVec<[(&UseItem<'db>, &Tree<'db>, Entry); 2]> = globs
                .iter()
                .filter_map(|(item, tree, from)| {
                    let per = from.def_map(db).module(db, *from).name_in_scope(db, name);
                    let it = entry(&per, ns)?;
                    it.vis.is_visible_from(db, module).then_some((*item, *tree, it))
                })
                .collect(); // alloc: inline, at most one per glob of the module, which has two at most almost always
            let Some(&(first_item, first_tree, first)) = imported.first() else { continue };
            match it {
                it if matches!(it.import, Some(Binding::Glob(_))) => {
                    let public = first_item.public && first.vis == Visibility::Public;
                    let exported = it.import.is_some_and(|it| uses.exported_names.contains(&(it, *name, ns)));
                    if !public || !exported {
                        continue;
                    }
                    let Some(&(_, other_tree, _)) = imported.iter().find(|(_, _, it)| it.def != first.def) else {
                        continue;
                    };
                    let root = first_item.trees.first().map(|it| it.node.syntax().text_range());
                    let Some(root) = root else { continue };
                    let span = cx.span(InFile::new(first_item.node.file_id, root));
                    // alloc: the lint fires; the name goes into its labels and fact
                    let text = name.as_str().to_owned();
                    // clone: the lint fires; its span is both the primary span and a label's
                    let mut diag = Diag::lint("ambiguous_glob_reexports", "ambiguous glob re-exports", span.clone());
                    diag.span_label(
                        span,
                        // alloc: a label of a lint that fires
                        format!("the name `{text}` in the {} namespace is first re-exported here", ns.descr()),
                    );
                    let other = cx.span(InFile::new(first_item.node.file_id, other_tree.node.syntax().text_range()));
                    diag.span_label(
                        other,
                        // alloc: a label of a lint that fires
                        format!("but the name `{text}` in the {} namespace is also re-exported here", ns.descr()),
                    );
                    // alloc: the fact of a lint that fires
                    diag.fact(cx.resolves_fact(module.into(), &text), format!("what `{text}` is in the module"));
                    let at = InFile::new(first_item.node.file_id, ast::ptr(first_item.node.value.syntax()));
                    found.push(Found::lint(diag, Phase::Resolve, at));
                }
                it => {
                    if it.vis == Visibility::Public || matches!(it.def, ModuleDefId::MacroId(MacroId::MacroRulesId(_))) {
                        continue;
                    }
                    let Some(&(glob_item, glob_tree, _)) = imported.iter().find(|(item, tree, it)| {
                        item.public
                            && it.vis == Visibility::Public
                            && tree.binding.is_some_and(|it| uses.exported.contains(&it))
                    }) else {
                        continue;
                    };
                    let Some(private) = binding_node(cx, uses, it) else { continue };
                    // alloc: the lint fires; the name goes into its note and fact
                    let text = name.as_str().to_owned();
                    let span = cx.span(private.map(item_span));
                    let mut diag =
                        // clone: the lint fires; its span is both the primary span and a note's
                        Diag::lint("hidden_glob_reexports", "private item shadows public glob re-export", span.clone());
                    let glob = cx.span(InFile::new(glob_item.node.file_id, glob_tree.node.syntax().text_range()));
                    diag.span_note(
                        glob,
                        // alloc: a note of a lint that fires
                        format!(
                            "the name `{text}` in the {} namespace is supposed to be publicly re-exported here",
                            ns.descr()
                        ),
                    );
                    diag.span_note(span, "but the private item here shadows it");
                    // alloc: the fact of a lint that fires
                    diag.fact(cx.resolves_fact(module.into(), &text), format!("what `{text}` is in the module"));
                    found.push(Found::lint(diag, Phase::Resolve, private.map(ast::ptr)));
                }
            }
        }
    }
    found
}

/// The node of what a name is bound to in a module, when it is not a glob's: the item that
/// defines it, the `use` tree or the `extern crate` item that imports it.
fn binding_node<'db>(cx: &Cx<'_, 'db>, uses: &Uses<'_, 'db>, it: Entry) -> Option<InFile<Node<'db>>> {
    match it.import {
        Some(binding @ Binding::Import(_)) => {
            let (item, tree) = uses.tree(binding)?;
            Some(InFile::new(item.node.file_id, tree.node.syntax()))
        }
        Some(Binding::ExternCrate(id)) => {
            let krate = uses.extern_crate_of(id)?;
            Some(krate.node.map(|it| it.syntax()))
        }
        Some(Binding::Glob(_)) => None,
        None => def_node(cx, it.def),
    }
}

/// The item that defines `def`.
fn def_node<'db>(cx: &Cx<'_, 'db>, def: ModuleDefId) -> Option<InFile<Node<'db>>> {
    fn node<'db, N: ast::AstNode<'db>>(it: Option<InFile<N>>) -> Option<InFile<Node<'db>>> {
        it.map(|it| it.map(|it| it.syntax()))
    }
    let db = cx.db;
    match ModuleDef::from(def) {
        ModuleDef::Module(it) => node(it.declaration_source_view(db)),
        ModuleDef::Function(it) => node(it.source_view(db)),
        ModuleDef::Adt(hir::Adt::Struct(it)) => node(it.source_view(db)),
        ModuleDef::Adt(hir::Adt::Union(it)) => node(it.source_view(db)),
        ModuleDef::Adt(hir::Adt::Enum(it)) => node(it.source_view(db)),
        ModuleDef::Const(it) => node(it.source_view(db)),
        ModuleDef::Static(it) => node(it.source_view(db)),
        ModuleDef::Trait(it) => node(it.source_view(db)),
        ModuleDef::TypeAlias(it) => node(it.source_view(db)),
        ModuleDef::Macro(it) => node(it.source_view(db)),
        ModuleDef::EnumVariant(_) | ModuleDef::BuiltinType(_) => None,
    }
}

/// rustc's span of a binding: an item's without its attributes, a `use` tree's whole.
fn item_span(node: Node<'_>) -> syntax::TextRange {
    if ast::UseTree::can_cast(node.kind()) { node.text_range() } else { item_range_view(node) }
}

/// The module rustc's privacy counts an item of `module` in: the nearest that is not a block.
fn nearest_module(cx: &Cx<'_, '_>, module: ModuleId) -> ModuleId {
    let db = cx.db;
    let mut at = module;
    while at.is_block_module(db) {
        match at.def_map(db).containing_module(db, at) {
            Some(it) => at = it,
            None => break,
        }
    }
    at
}

/// The visibility a `use` item is declared with, resolved.
fn item_visibility(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, item: &UseItem<'_>) -> Option<Vis> {
    let db = cx.db;
    let home = nearest_module(cx, item.module);
    let Some(vis) = item.node.value.visibility() else { return Some(Vis::Module(home)) };
    Some(match vis.kind() {
        ast::VisibilityKind::Pub => Vis::Public,
        ast::VisibilityKind::PubCrate => Vis::Crate,
        ast::VisibilityKind::PubSelf => Vis::Module(home),
        ast::VisibilityKind::PubSuper => Vis::Module(home.def_map(db).containing_module(db, home)?),
        ast::VisibilityKind::In(path) => {
            let span_map = item.node.file_id.span_map(db);
            let path = ModPath::from_src_view(db, path, &mut |range| span_map.span_for_range(range).ctx)?;
            match entry(&uses.lookup(cx, home, path.kind, path.segments()), Ns::Types)?.def {
                ModuleDefId::ModuleId(it) => Vis::module(cx, it),
                _ => return None,
            }
        }
    })
}

/// A visibility as rustc has it: public, the crate's, or restricted to a module that is not
/// the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vis {
    Public,
    Crate,
    Module(ModuleId),
}

impl Vis {
    fn of(cx: &Cx<'_, '_>, vis: Visibility) -> Vis {
        match vis {
            Visibility::Public => Vis::Public,
            Visibility::PubCrate(_) => Vis::Crate,
            Visibility::Module(module, _) => Vis::module(cx, module),
        }
    }

    /// Visible in `module`, the nearest module that is not a block.
    fn module(cx: &Cx<'_, '_>, module: ModuleId) -> Vis {
        let module = nearest_module(cx, module);
        if module.def_map(cx.db).module(cx.db, module).parent.is_none() && !module.is_block_module(cx.db) {
            Vis::Crate
        } else {
            Vis::Module(module)
        }
    }

    /// Whether `a` is at least `b`: everywhere `b` is visible, `a` is.
    fn at_least(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> bool {
        match (a, b) {
            (Vis::Public, _) => true,
            (_, Vis::Public) => false,
            (Vis::Crate, _) => true,
            (_, Vis::Crate) => false,
            (Vis::Module(a), Vis::Module(b)) => {
                let mut at = Some(b);
                while let Some(module) = at {
                    if module == a {
                        return true;
                    }
                    at = module.def_map(cx.db).module(cx.db, module).parent;
                }
                false
            }
        }
    }

    fn greater_than(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> bool {
        a != b && Vis::at_least(cx, a, b)
    }

    fn min(cx: &Cx<'_, '_>, a: Vis, b: Vis) -> Vis {
        if Vis::at_least(cx, b, a) { a } else { b }
    }

    /// rustc's `Visibility::to_string` for an item of `home`, for the message of a lint that
    /// fires.
    fn to_string(self, cx: &Cx<'_, '_>, home: ModuleId) -> String {
        match self {
            // alloc: the message of a lint that fires
            Vis::Public => "pub".to_owned(),
            // alloc: the message of a lint that fires
            Vis::Crate => "pub(crate)".to_owned(),
            // alloc: the message of a lint that fires
            Vis::Module(module) if module == home => "pub(self)".to_owned(),
            Vis::Module(module) => {
                let mut names: SmallVec<[Name; 8]> = SmallVec::new();
                let mut at = Some(module);
                while let Some(it) = at {
                    if let Some(name) = it.name(cx.db) {
                        names.push(name);
                    }
                    at = it.def_map(cx.db).module(cx.db, it).parent;
                }
                // alloc: the message of a lint that fires
                let mut path = String::with_capacity(32);
                for (i, name) in names.iter().rev().enumerate() {
                    if i > 0 {
                        path.push_str("::");
                    }
                    path.push_str(name.as_str());
                }
                // alloc: the message of a lint that fires
                format!("pub(in crate::{path})")
            }
        }
    }
}
