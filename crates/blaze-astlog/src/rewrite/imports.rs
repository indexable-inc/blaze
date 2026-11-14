//! The imports a retarget moves. A bare name the retarget writes as the new item's name needs
//! the new item imported; the `use` that brought the old item goes once nothing else reads
//! what it imports. A `use` leaf a rule retargets itself is that same import, so it is moved
//! here, never rewritten in place, where a path in braces is relative to the tree's prefix.
//!
//! The new item is imported beside the old item's `use` when there is one, in that `use`'s
//! scope (a module or a block), and else at the end of the module's `use`s. A read of the old
//! import is a first path segment that resolves to the old item, or any token of its name in a
//! macro's input or a `use` path, which can't be resolved here and so counts; a read a retarget
//! of the request rewrote does not. What would make the import wrong is refused, naming it:
//! the new name taken where a moved name is, or a retargeted `use` still read.

use blaze_api::Edit;
use blaze_intent::retarget::{Names, names_in, reimport};
use blaze_intent::{Files, TextEdit};
use hir::{FindPathConfig, ModPath, ModuleDef, ScopeDef};
use std::{
    alloc::{Allocator, Global},
    ops::Deref,
};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _},
};

use super::{Made, Origin, Res, locate, place, unplaceable};
use crate::sema::{Def, Sema};

/// A bare path a retarget wrote: the path, the new item, and whether its name was written in
/// place of a longer path, so the item must be imported.
pub(super) struct Bare<'a, 'p> {
    pub path: ast::Node<'a>,
    pub to: ModuleDef,
    pub import: bool,
    pub by: Origin<'p>,
}

/// What a request's retargets did, for the imports they move.
#[derive(Default)]
pub(super) struct Moved<'a, 'p> {
    pub bare: Vec<Bare<'a, 'p>>,
    /// Each `use` leaf a rule retargets, with the new item.
    pub leaves: Vec<(ast::UseTree<'a>, ModuleDef, Origin<'p>)>,
    /// The text each retarget rewrote, by file.
    pub covered: Vec<(String, TextRange)>,
}

/// `path`, a path in a use tree that a rule retargets to `to`, at `at` in its file: recorded
/// when it is the whole path of a private leaf of a file's `use`; else an error saying why the
/// retarget can't move it.
pub(super) fn leaf<'a, 'p, F: Files>(
    facts: &Sema<'a, F>,
    by: Origin<'p>,
    path: ast::Node<'a>,
    at: (&str, TextRange),
    to: ModuleDef,
    moved: &mut Moved<'a, 'p>,
) -> Res<()> {
    let (file, range) = at;
    let fail = |message: String| unplaceable(by, place(facts.files, file, range.start()), message);
    let tree = path.parent().and_then(ast::UseTree::cast).filter(|tree| {
        tree.path().is_some_and(|it| it.syntax() == path)
            && tree.use_tree_list().is_none()
            && tree.star_token().is_none()
    });
    let Some(tree) = tree else {
        return Err(fail(format!(
            "retargets `{}`, which a use tree imports through (a prefix or a glob): retarget \
             the items it imports, or move the import by hand",
            path.text()
        )));
    };
    if facts.tree_at(facts.tree(path)).call.is_some() {
        return Err(fail(format!("retargets the import `{}`, which a macro call holds", path.text())));
    }
    if tree.syntax().ancestors().find_map(ast::Use::cast).is_none_or(is_pub) {
        return Err(fail(format!(
            "retargets `{}`, a re-export other modules may read: write its new `pub use` by hand",
            path.text()
        )));
    }
    moved.covered.push((file.to_owned(), range));
    moved.leaves.push((tree, to, by));
    Ok(())
}

/// Where the imports of one group go: beside the old item's `use` leaf, or in a module.
#[derive(PartialEq)]
enum Key<'a> {
    Leaf(ast::UseTree<'a>),
    Module { file: String, into: Option<String> },
}

/// A path whose new item a group imports: the path, the item, what the path named before, and
/// the rule.
type Needs<'a, 'p> = (ast::Node<'a>, ModuleDef, Names, Origin<'p>);

/// The bare paths and retargeted leaves whose imports go to one place.
struct Group<'a, 'p> {
    key: Key<'a>,
    file: String,
    by: Origin<'p>,
    /// The new items to import, each by its absolute path.
    imports: Vec<(String, ModuleDef)>,
    needs: Vec<Needs<'a, 'p>>,
    /// The rule that retargets the leaf itself, when one does.
    retargeted: Option<Origin<'p>>,
}

/// The group of `key`, made when it is the first of it.
fn group_of<'g, 'a, 'p, A: Allocator>(
    groups: &'g mut Vec<Group<'a, 'p>, A>,
    key: Key<'a>,
    file: &str,
    by: Origin<'p>,
) -> &'g mut Group<'a, 'p> {
    let at = match groups.iter().position(|it| it.key == key) {
        Some(at) => at,
        None => {
            let file = file.to_owned();
            groups.push(Group { key, file, by, imports: Vec::new(), needs: Vec::new(), retargeted: None });
            groups.len() - 1
        }
    };
    &mut groups[at]
}

/// The imports `moved` asks for, as text edits of the `use`s beside an old import and as
/// `Edit::AddUse`s of a module.
pub(super) fn finish<'a, 'p, F: Files, A: Allocator, B: Allocator>(
    facts: &Sema<'a, F>,
    moved: Moved<'a, 'p>,
    text: &mut Vec<Made<'p>, A>,
    edits: &mut Vec<Edit, B>,
) -> Res<()> {
    let mut groups = Vec::new_in(Global);
    for bare in &moved.bare {
        let old = names_of(facts, bare.path);
        let in_file = in_file(facts, bare.path);
        let (file, _) = locate(facts, in_file)
            .map_err(|call| unplaceable(bare.by, call, "retargets a path a macro made"))?;
        let key = match leaf_of(facts, in_file, bare.path.text(), &old) {
            Some(leaf) => Key::Leaf(leaf),
            None => {
                let into = in_file.ancestors().find(|it| it.kind() == SyntaxKind::MODULE);
                Key::Module { file: file.clone(), into: into.map(blaze_intent::path::of) }
            }
        };
        let import = match bare.import {
            true => {
                let import = import_path(facts, bare.by, bare.path, bare.to)?;
                Some((import.text(facts.db).to_string(), bare.to))
            }
            false => None,
        };
        let group = group_of(&mut groups, key, &file, bare.by);
        if let Some(import) = import {
            add(&mut group.imports, import);
            group.needs.push((bare.path, bare.to, old, bare.by));
        }
    }
    for &(leaf, to, by) in &moved.leaves {
        let (file, _) = locate(facts, leaf.syntax())
            .map_err(|call| unplaceable(by, call, "retargets an import a macro made"))?;
        let import = (import_path(facts, by, leaf.syntax(), to)?.text(facts.db).to_string(), to);
        let path = leaf.path().map_or(leaf.syntax(), |it| it.syntax());
        let old = names_of(facts, path);
        let group = group_of(&mut groups, Key::Leaf(leaf), &file, by);
        add(&mut group.imports, import);
        group.needs.push((path, to, old, by));
        group.retargeted = Some(by);
    }
    for group in groups {
        place_group(facts, &moved.covered, group, text, edits)?;
    }
    Ok(())
}

/// `item` pushed to `list` unless it is there.
fn add<T: PartialEq, A: Allocator>(list: &mut Vec<T, A>, item: T) {
    if !list.contains(&item) {
        list.push(item);
    }
}

/// One group's imports, made: the leaf's `use` rewritten, or the module's `use`s added.
fn place_group<'a, 'p, F: Files, S: Deref<Target = str>, A: Allocator, B: Allocator>(
    facts: &Sema<'a, F>,
    covered: &[(S, TextRange)],
    group: Group<'a, 'p>,
    text: &mut Vec<Made<'p>, A>,
    edits: &mut Vec<Edit, B>,
) -> Res<()> {
    let Group { key, file, by, imports, needs, retargeted } = group;
    for (at, (path, def)) in imports.iter().enumerate() {
        let name = def.name(facts.db);
        if let Some((other, _)) = imports[..at].iter().find(|(_, it)| it.name(facts.db) == name) {
            return Err(unplaceable(
                by,
                file.clone(),
                format!("`{other}` and `{path}` would both be imported here under one name"),
            ));
        }
    }
    let leaf = match key {
        Key::Leaf(leaf) => leaf,
        Key::Module { into, .. } => {
            for &(path, to, _, by) in &needs {
                taken(facts, by, path, to, None)?;
            }
            for (path, _) in imports {
                add(edits, Edit::AddUse { file: file.clone(), into: into.clone(), path });
            }
            return Ok(());
        }
    };
    let at = place(facts.files, &file, leaf.syntax().text_range().start());
    let Some(item) = leaf.syntax().ancestors().find_map(ast::Use::cast) else {
        return Err(unplaceable(by, at, "a use tree outside a `use`"));
    };
    let name = imported_name(leaf);
    let old = leaf.path().map_or([None, None, None], |it| names_of(facts, it.syntax()));
    let read = match (item.syntax().parent(), name) {
        (Some(scope), Some(name)) => first_read(facts, scope, leaf, name, &old, (&file, covered)),
        _ => None,
    };
    if let (Some(read), Some(by)) = (read, retargeted) {
        return Err(unplaceable(
            by,
            at,
            format!(
                "retargets the import `{}`, which `{}` at {} still reads, and no rule retargets \
                 that",
                leaf.syntax().text(),
                name.unwrap_or_default(),
                place(facts.files, &file, read.start())
            ),
        ));
    }
    let dropped = read.is_none();
    for (path, to, old, by) in &needs {
        taken(facts, *by, *path, *to, dropped.then_some(old))?;
    }
    if !dropped && imports.is_empty() {
        return Ok(());
    }
    let mut paths = Vec::new_in(Global);
    paths.extend(imports.into_iter().map(|(path, _)| path));
    let root = item.syntax().ancestors().last().unwrap_or(item.syntax());
    let rewrite = reimport(item, root, dropped.then_some(leaf), &paths)
        .map_err(|why| unplaceable(by, at.clone(), why.to_string()))?;
    let edit = TextEdit { path: file, range: rewrite.range, text: rewrite.text, by: by.by() };
    text.push(Made { edit, by });
    Ok(())
}

/// The items `path` names, each namespace's; none for a node that is no path.
fn names_of<'a, F: Files>(facts: &Sema<'a, F>, path: ast::Node<'a>) -> Names {
    match ast::Path::cast(path) {
        Some(path) => names_in(&facts.sema, path),
        None => [None, None, None],
    }
}

/// Whether two namespace triples share an item.
fn meets(a: &Names, b: &Names) -> bool {
    a.iter().flatten().any(|it| b.contains(&Some(*it)))
}

/// `node` in its file: a node of a macro's expansion is the macro call's, outward.
fn in_file<'a, F: Files>(facts: &Sema<'a, F>, node: ast::Node<'a>) -> ast::Node<'a> {
    let mut node = node;
    while let Some(call) = facts.tree_at(facts.tree(node)).call.and_then(|it| facts.view_of(it)) {
        node = call;
    }
    node
}

/// Whether a `use` has a visibility: a re-export, read from outside the file.
fn is_pub(item: ast::Use<'_>) -> bool {
    item.syntax().children().any(|it| it.kind() == SyntaxKind::VISIBILITY)
}

/// The name a `use` leaf imports: its rename, or its path's last segment; `None` for `_`.
fn imported_name(leaf: ast::UseTree<'_>) -> Option<&str> {
    let name = match leaf.rename() {
        Some(rename) => rename.syntax().text().trim_start_matches("as").trim(),
        None => leaf.path()?.segment()?.name_ref()?.syntax().text(),
    };
    (name != "_").then_some(name)
}

/// The private `use` leaf of the file that imports `name` as an item of `old` into a scope of
/// `node`, innermost first, up to `node`'s module.
fn leaf_of<'a, F: Files>(
    facts: &Sema<'a, F>,
    node: ast::Node<'a>,
    name: &str,
    old: &Names,
) -> Option<ast::UseTree<'a>> {
    for scope in node.ancestors() {
        if scope.kind() == SyntaxKind::MODULE {
            return None;
        }
        for item in scope.children().filter_map(ast::Use::cast).filter(|it| !is_pub(*it)) {
            let found = item.syntax().descendants().filter_map(ast::UseTree::cast).find(|leaf| {
                leaf.use_tree_list().is_none()
                    && leaf.star_token().is_none()
                    && imported_name(*leaf) == Some(name)
                    && leaf.path().is_some_and(|it| meets(&names_of(facts, it.syntax()), old))
            });
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

/// The first read in `scope` of `name`, which `leaf` imports as an item of `old`, that no
/// retarget of the request rewrote (`covered`, in `file`).
fn first_read<'a, F: Files, S: Deref<Target = str>>(
    facts: &Sema<'a, F>,
    scope: ast::Node<'a>,
    leaf: ast::UseTree<'a>,
    name: &str,
    old: &Names,
    (file, covered): (&str, &[(S, TextRange)]),
) -> Option<TextRange> {
    let own = leaf.syntax().text_range();
    scope.descendants_with_tokens().find_map(|it| {
        let NodeOrToken::Token(token) = it else { return None };
        let range = token.text_range();
        if token.kind() != SyntaxKind::IDENT || token.text() != name || own.contains_range(range) {
            return None;
        }
        if covered.iter().any(|(path, it)| &**path == file && it.contains_range(range)) {
            return None;
        }
        let parent = token.parent()?;
        let read = match parent.kind() {
            SyntaxKind::NAME => false,
            SyntaxKind::NAME_REF => {
                let path = parent.parent().filter(|it| it.kind() == SyntaxKind::PATH_SEGMENT);
                match path.and_then(|it| it.parent()).and_then(ast::Path::cast) {
                    // A field, a method or a label: no path, no read of an import.
                    None => false,
                    // A later segment reads what the path before it names.
                    Some(path) if path.qualifier().is_some() => false,
                    Some(path) if path.syntax().ancestors().any(|it| it.kind() == SyntaxKind::USE_TREE) => true,
                    Some(path) => meets(&names_in(&facts.sema, path), old),
                }
            }
            _ => true,
        };
        read.then_some(range)
    })
}

/// A path to import, and the crate it is written in, whose edition spells it.
struct Import {
    path: ModPath,
    krate: hir::Crate,
}

impl Import {
    fn text<'d>(&'d self, db: &'d base_db::Db) -> impl std::fmt::Display + 'd {
        self.path.display(db, self.krate.edition(db))
    }
}

/// The new item's path to import from where `node` is: from its crate's root, or its crate.
fn import_path<'a, F: Files>(
    facts: &Sema<'a, F>,
    by: Origin<'_>,
    node: ast::Node<'a>,
    to: ModuleDef,
) -> Res<Import> {
    let at = || match locate(facts, node) {
        Ok((path, range)) => place(facts.files, &path, range.start()),
        Err(call) => call,
    };
    let Some(scope) = facts.sema.scope(node) else {
        return Err(unplaceable(by, at(), "is in no module"));
    };
    let config = FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: true,
        prefer_absolute: true,
        allow_unstable: true,
    };
    let Some(path) = scope.module().find_path(facts.db, to, config) else {
        let name = facts.def_path(Def::Item(to)).unwrap_or_default();
        return Err(unplaceable(by, at(), format!("this module can import no path to `{name}`")));
    };
    Ok(Import { path, krate: scope.krate() })
}

/// An error when `to`'s name, imported where `path` is, would clash with or shadow what the
/// name means there; what `old` names is no clash when its import goes.
fn taken<'a, F: Files>(
    facts: &Sema<'a, F>,
    by: Origin<'_>,
    path: ast::Node<'a>,
    to: ModuleDef,
    old: Option<&Names>,
) -> Res<()> {
    let at = || match locate(facts, path) {
        Ok((file, range)) => place(facts.files, &file, range.start()),
        Err(call) => call,
    };
    let Some(scope) = facts.sema.scope(path) else {
        return Err(unplaceable(by, at(), "is in no module"));
    };
    let Some(name) = to.name(facts.db) else { return Ok(()) };
    let mut clash: Option<ScopeDef<'_>> = None;
    scope.process_all_names(&mut |it, def| {
        if clash.is_some() || it.as_str() != name.as_str() {
            return;
        }
        let same = match def {
            ScopeDef::ModuleDef(def) => def == to || old.is_some_and(|old| old.contains(&Some(def))),
            _ => false,
        };
        if !same {
            clash = Some(def);
        }
    });
    let Some(clash) = clash else { return Ok(()) };
    let what = match clash {
        ScopeDef::ModuleDef(def) => {
            facts.def_path(Def::Item(def)).map_or_else(|| "an item".to_owned(), |it| format!("`{it}`"))
        }
        ScopeDef::Local(_) => "a local".to_owned(),
        ScopeDef::GenericParam(_) => "a generic parameter".to_owned(),
        _ => "another name".to_owned(),
    };
    let to = facts.def_path(Def::Item(to)).unwrap_or_default();
    Err(unplaceable(
        by,
        at(),
        format!(
            "`{}` already names {what} here, so `{to}` can't be imported: retarget to a \
             qualified path, or rename one of them",
            name.as_str()
        ),
    ))
}
