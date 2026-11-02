//! Retargeting: every reference of one file to some items, moved to other paths, each found by
//! the front end's name resolution, never by text search. It is one edit with two callers: a
//! lint's machine-applicable fix (a `disallowed-types` entry moves `std::sync::Mutex` to
//! `parking_lot::Mutex`) and a request's replace (`blaze check --replace A=B`).
//!
//! A `use` item that imports a moved item is rewritten whole: its other leaves stay as they are
//! written, and each moved leaf becomes an import of its own of the new path, with the item's
//! attributes and visibility, and its rename. A leaf whose new path ends in another name is
//! imported under its old name (`use crossbeam_channel::Sender as SyncSender;`), so no other
//! line of the file has to change. A glob (`use std::sync::*;`) stays, and the moved items of
//! its module the file names get imports beside it, which shadow the glob's. A path outside any
//! `use` that qualifies a moved item (`std::sync::Mutex::new`) is rewritten up to the item's
//! name; its generic arguments stay. A path of one segment names the item through an import or
//! a glob, which the rewrite of that import moves.
//!
//! A path in a macro's input is no node of the file's tree, so [`Retarget::file`] passes over
//! the tokens: the caller that sees expansions rewrites what they name ([`Retarget::path`] of
//! an expanded path, at the range it maps to). What the rewrite can't place is an error naming
//! where it is: a comment inside a use tree it rewrites.

use hir::{Module, ModuleDef, PathResolution, ScopeDef, view::Semantics};
use syntax::{
    SyntaxKind, TextRange, WalkEvent,
    view::{self as ast, AstNode as _},
};

/// One rewrite of a file's base text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rewrite {
    pub range: TextRange,
    pub text: String,
}

/// What a path names, in the type and the value namespace.
pub type Names = [Option<ModuleDef>; 2];

/// The references to move, and how the front end resolves them in the file.
pub struct Retarget<'r, 't> {
    pub db: &'r base_db::Db,
    /// What a path of the file names; a path in a use tree with the tree's prefix.
    pub resolve: &'r dyn Fn(ast::Path<'t>) -> Names,
    /// The new path of an item that moves (`parking_lot::Mutex`); `None` for one that stays.
    pub to: &'r dyn Fn(ModuleDef) -> Option<String>,
}

impl<'t> Retarget<'_, 't> {
    /// Every rewrite of the file `root` is: each `use` item importing a moved item, and each
    /// outermost path outside a `use` qualifying one, in the file's order, none overlapping.
    pub fn file(&self, root: ast::Node<'t>) -> Result<Vec<Rewrite>, String> {
        let mut out = Vec::new();
        let mut events = root.preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            if let Some(item) = ast::Use::cast(node) {
                events.skip_subtree();
                out.extend(self.use_item(item, root)?);
                continue;
            }
            if node.kind() == SyntaxKind::TOKEN_TREE {
                events.skip_subtree();
                continue;
            }
            if let Some(path) = ast::Path::cast(node)
                && let Some(rewrite) = self.path(path)
            {
                events.skip_subtree();
                out.push(rewrite);
            }
        }
        Ok(out)
    }

    /// The new path of what `path` names, in the type namespace first.
    fn moved(&self, path: ast::Path<'t>) -> Option<(ModuleDef, String)> {
        let [ty, value] = (self.resolve)(path);
        [ty, value].into_iter().flatten().find_map(|def| Some((def, (self.to)(def)?)))
    }

    /// The rewrite of `path`, a path outside any `use`, when it qualifies a moved item: from its
    /// start to the end of the item's name.
    pub fn path(&self, path: ast::Path<'t>) -> Option<Rewrite> {
        path.qualifier()?;
        if path.syntax().ancestors().any(|it| it.kind() == SyntaxKind::USE_TREE) {
            return None;
        }
        let (_, to) = self.moved(path)?;
        let name = path.segment()?.name_ref()?;
        let range =
            TextRange::new(path.syntax().text_range().start(), name.syntax().text_range().end());
        Some(Rewrite { range, text: to })
    }

    /// The rewrite of `item`, when a leaf of it imports a moved item or a glob of it imports one
    /// that `root`, its file, names.
    pub fn use_item(
        &self,
        item: ast::Use<'t>,
        root: ast::Node<'t>,
    ) -> Result<Option<Rewrite>, String> {
        let Some(tree) = item.use_tree() else { return Ok(None) };
        let mut moved: Vec<String> = Vec::new();
        let kept = self.keep(tree, root, &mut moved)?;
        if moved.is_empty() {
            return Ok(None);
        }
        if tree.syntax().descendants_with_tokens().any(|it| it.kind() == SyntaxKind::COMMENT) {
            return Err(format!(
                "`{}` has a comment inside its tree, which rewriting the tree would drop: move it \
                 out of the braces",
                item.syntax().text()
            ));
        }
        let use_token = item.use_token().ok_or("a `use` item without `use`")?;
        let start = item.syntax().text_range().start();
        let head = &item.syntax().text()[..usize::from(use_token.text_range().start() - start)];
        let indent = indent_of(root, start);
        let mut lines = Vec::new();
        if let Some(kept) = kept {
            lines.push(format!("{head}use {kept};"));
        }
        for path in moved {
            lines.push(format!("{head}use {path};"));
        }
        let text = lines.join(&format!("\n{indent}"));
        Ok(Some(Rewrite { range: item.syntax().text_range(), text }))
    }

    /// `tree` as written with its moved leaves taken out, each pushed to `moved` as the path
    /// (and rename) of its new import; `None` when nothing of it stays.
    fn keep(
        &self,
        tree: ast::UseTree<'t>,
        root: ast::Node<'t>,
        moved: &mut Vec<String>,
    ) -> Result<Option<String>, String> {
        let prefix = tree.path().map(|it| it.syntax().text());
        if let Some(list) = tree.use_tree_list() {
            let mut kept = Vec::new();
            for child in list.use_trees() {
                kept.extend(self.keep(child, root, moved)?);
            }
            return Ok(match (prefix, kept.as_slice()) {
                (_, []) => None,
                (Some(prefix), [one]) if one != "self" && !one.starts_with('{') => {
                    Some(format!("{prefix}::{one}"))
                }
                (Some(prefix), _) => Some(format!("{prefix}::{{{}}}", kept.join(", "))),
                (None, _) => Some(format!("{{{}}}", kept.join(", "))),
            });
        }
        let written = tree.syntax().text().to_owned();
        if tree.star_token().is_some() {
            if let Some(path) = tree.path() {
                let item = tree.syntax().ancestors().find(|it| it.kind() == SyntaxKind::USE);
                self.glob(path, item.and_then(|it| it.parent()).unwrap_or(root), moved);
            }
            return Ok(Some(written));
        }
        let Some(path) = tree.path() else { return Ok(Some(written)) };
        let Some((_, to)) = self.moved(path) else { return Ok(Some(written)) };
        let old = path.segment().and_then(|it| it.name_ref()).map(|it| it.text().to_owned());
        let alias = match tree.rename() {
            Some(rename) => Some(rename.syntax().text().trim_start_matches("as").trim().to_owned()),
            None => old.filter(|old| to.rsplit("::").next() != Some(old.as_str())),
        };
        moved.push(match alias {
            Some(alias) => format!("{to} as {alias}"),
            None => to,
        });
        Ok(None)
    }

    /// The imports of the moved items of the module a glob `path` imports that `scope`, what the
    /// glob's `use` is in, names by one segment, each under its name in the module.
    fn glob(&self, path: ast::Path<'t>, scope: ast::Node<'t>, moved: &mut Vec<String>) {
        let Some(ModuleDef::Module(module)) = (self.resolve)(path)[0] else { return };
        for (name, def) in Module::scope(module, self.db, None) {
            let ScopeDef::ModuleDef(def) = def else { continue };
            let Some(to) = (self.to)(def) else { continue };
            if !self.names(scope, name.as_str(), def) {
                continue;
            }
            let import = match to.rsplit("::").next() == Some(name.as_str()) {
                true => to,
                false => format!("{to} as {}", name.as_str()),
            };
            if !moved.contains(&import) {
                moved.push(import);
            }
        }
    }

    /// Whether a path of one segment in `scope`, outside any `use` and any module inside it
    /// (which a glob of `scope` doesn't reach), is `name` and names `def`.
    pub fn names(&self, scope: ast::Node<'t>, name: &str, def: ModuleDef) -> bool {
        let mut events = scope.preorder();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(node) = event else { continue };
            if node != scope && matches!(node.kind(), SyntaxKind::MODULE | SyntaxKind::USE) {
                events.skip_subtree();
                continue;
            }
            if let Some(path) = ast::Path::cast(node)
                && path.qualifier().is_none()
                && path.segment().and_then(|it| it.name_ref()).is_some_and(|it| it.text() == name)
                && (self.resolve)(path).contains(&Some(def))
            {
                return true;
            }
        }
        false
    }
}

/// What `path` names as `sema` resolves it: [`Retarget::resolve`] for a file outside any body's
/// analysis.
pub fn names_in<'db>(sema: &Semantics<'db>, path: ast::Path<'db>) -> Names {
    let Some(per_ns) = sema.resolve_path_per_ns(path) else { return [None, None] };
    let def = |it: Option<PathResolution<'db>>| match it {
        Some(PathResolution::Def(it)) => Some(it),
        _ => None,
    };
    [def(per_ns.type_ns), def(per_ns.value_ns)]
}

/// The whitespace a line starts with before `at`.
fn indent_of(root: ast::Node<'_>, at: syntax::TextSize) -> &str {
    let text = root.file().text();
    let at = usize::from(at);
    let line = text[..at].rfind('\n').map_or(0, |it| it + 1);
    let lead = &text[line..at];
    match lead.trim_start_matches([' ', '\t']).is_empty() {
        true => lead,
        false => "",
    }
}
