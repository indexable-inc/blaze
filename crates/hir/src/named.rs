//! The dependencies a crate's code names, which are the ones rustc loads for it and so the ones
//! a program links (rustc's `CrateLoader`): a crate given with `--extern` is loaded only when a
//! path looks it up in the extern prelude or an `extern crate` names it, and a crate that is not
//! loaded is not linked, nor are its `#[used]` statics, its constructors or its
//! `#[global_allocator]`. Cargo hands a target every dependency of its package, and a binary its
//! package's library, whether it uses them or not.
//!
//! What is read is the syntax of every file of the crate and of every macro expansion in it, at
//! the level of items and in bodies, leaving out what a `#[cfg]` that is false leaves out. The
//! names looked up in the extern prelude are among the first segments of those paths: of every
//! path of more than one segment and of a global one, and of a path of one segment that is a type
//! or an import (a single segment in an expression, a pattern, an attribute or a macro call is
//! looked up in the value or macro namespace, which the extern prelude is not in). In a token tree
//! that is not parsed (a macro's input, an attribute's arguments) an identifier next to `::` is
//! taken as such a segment. So more can be named than rustc looks up, never less.
//!
//! The files are read before any macro is expanded, and expanding stops once every dependency is
//! named: most crates name each in a `use`, and expanding a macro of a body lowers the body.

use std::sync::Arc;

use base_db::CrateOrigin;
use hir_expand::HirFileId;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, T, WalkEvent,
    ast::{self, HasAttrs},
};

use crate::{Crate, Semantics, db::HirDatabase};

/// The dependencies of `krate` that its code names, as rustc loads them, with those of the
/// standard library's that it is given (rustc injects `std` or `core`, and blaze builds the
/// standard library as a whole): every one, for a crate of the standard library. A proc-macro
/// crate is among them when it is named, though a program does not link one.
pub fn named_dependencies<DB: HirDatabase>(db: &DB, krate: Crate) -> Vec<Crate> {
    if krate.is_builtin(db) {
        return krate.dependencies(db).into_iter().map(|it| it.krate).collect();
    }
    let mut named = Vec::new();
    let mut unnamed: FxHashMap<String, Vec<Crate>> = FxHashMap::default();
    for dep in krate.dependencies(db) {
        match dep.krate.origin(db) {
            CrateOrigin::Lang(_) => named.push(dep.krate),
            _ => unnamed.entry(dep.name.as_str().to_owned()).or_default().push(dep.krate),
        }
    }
    if unnamed.is_empty() {
        return named;
    }
    let sema = Semantics::new_within(db, Arc::new([krate.id].into_iter().collect()));
    let mut walk = Walk {
        sema: &sema,
        cfg: krate.id.cfg_options(db),
        unnamed,
        named: &mut named,
        read: FxHashSet::default(),
        unexpanded: Vec::new(),
    };
    for module in krate.modules(db) {
        walk.file(module.definition_source_file_id(db));
    }
    while !walk.unnamed.is_empty()
        && let Some(macro_) = walk.unexpanded.pop()
    {
        walk.expand(macro_);
    }
    named
}

/// A macro of the crate not expanded yet.
enum Unexpanded {
    Call(ast::MacroCall),
    Attr(ast::Item),
    Derive(ast::Meta),
}

struct Walk<'a, 'db, DB: HirDatabase> {
    sema: &'a Semantics<'db, DB>,
    cfg: &'db cfg::CfgOptions,
    /// The dependencies not named yet, by the name the crate has for them.
    unnamed: FxHashMap<String, Vec<Crate>>,
    named: &'a mut Vec<Crate>,
    /// The files and expansions read.
    read: FxHashSet<HirFileId>,
    unexpanded: Vec<Unexpanded>,
}

impl<DB: HirDatabase> Walk<'_, '_, DB> {
    fn file(&mut self, file: HirFileId) {
        if self.read.insert(file) {
            let root = self.sema.parse_or_expand(file);
            self.node(&root);
        }
    }

    fn expand(&mut self, macro_: Unexpanded) {
        match macro_ {
            Unexpanded::Call(call) => {
                if let Some(expanded) = self.sema.expand_macro_call(&call) {
                    self.file(expanded.file_id);
                }
            }
            Unexpanded::Attr(item) => {
                if let Some(expanded) = self.sema.expand_attr_macro(&item) {
                    self.file(expanded.value.file_id);
                }
            }
            Unexpanded::Derive(meta) => {
                for expanded in self.sema.expand_derive_macro(&meta).into_iter().flatten().flatten()
                {
                    self.node(&expanded.value);
                }
            }
        }
    }

    fn node(&mut self, root: &SyntaxNode) {
        let mut events = root.preorder_with_tokens();
        while let Some(event) = events.next() {
            if self.unnamed.is_empty() {
                return;
            }
            let WalkEvent::Enter(element) = event else { continue };
            let node = match element {
                NodeOrToken::Token(token) => {
                    if token.kind() == SyntaxKind::IDENT
                        && in_token_tree(&token)
                        && by_colons(&token)
                    {
                        self.name(token.text());
                    }
                    continue;
                }
                NodeOrToken::Node(node) => node,
            };
            if configured_out(&node, self.cfg) {
                events.skip_subtree();
                continue;
            }
            if let Some(path) = ast::Path::cast(node.clone()) {
                if let Some(head) = looked_up(&path) {
                    self.name(head.text().as_str());
                }
            } else if let Some(extern_crate) = ast::ExternCrate::cast(node.clone()) {
                if let Some(name) = extern_crate.name_ref() {
                    self.name(name.text().as_str());
                }
            } else if let Some(call) = ast::MacroCall::cast(node.clone()) {
                self.unexpanded.push(Unexpanded::Call(call));
            } else if let Some(item) = ast::Item::cast(node.clone()) {
                if let Some(adt) = ast::Adt::cast(node) {
                    for meta in adt.attrs().filter_map(|it| it.meta()) {
                        self.derives(meta);
                    }
                }
                if item.attrs().next().is_some() {
                    self.unexpanded.push(Unexpanded::Attr(item));
                }
            }
        }
    }

    /// The derives `meta` asks for, also under a `#[cfg_attr]` that is not false.
    fn derives(&mut self, meta: ast::Meta) {
        match meta {
            ast::Meta::CfgAttrMeta(ref under) => {
                let predicate = under.cfg_predicate().map(cfg::CfgExpr::parse_from_ast);
                if predicate.is_some_and(|it| self.cfg.check(&it) == Some(false)) {
                    return;
                }
                for meta in under.metas() {
                    self.derives(meta);
                }
            }
            meta if is_derive(&meta) => self.unexpanded.push(Unexpanded::Derive(meta)),
            _ => {}
        }
    }

    fn name(&mut self, text: &str) {
        let text = text.strip_prefix("r#").unwrap_or(text);
        if let Some(crates) = self.unnamed.remove(text) {
            self.named.extend(crates);
        }
    }
}

/// The first segment of `path` when it is one rustc may look up in the extern prelude.
fn looked_up(path: &ast::Path) -> Option<ast::NameRef> {
    if path.qualifier().is_some() {
        return None;
    }
    let segment = path.segment()?;
    let head = segment.name_ref()?;
    if head.self_token().is_some()
        || head.super_token().is_some()
        || head.crate_token().is_some()
        || head.Self_token().is_some()
    {
        return None;
    }
    let whole = path.syntax().ancestors().map_while(ast::Path::cast).last()?;
    if whole.qualifier().is_some() || segment.coloncolon_token().is_some() {
        return Some(head);
    }
    let parent = whole.syntax().parent()?;
    let elsewhere = matches!(
        parent.kind(),
        SyntaxKind::PATH_EXPR
            | SyntaxKind::PATH_PAT
            | SyntaxKind::TUPLE_STRUCT_PAT
            | SyntaxKind::MACRO_CALL
            | SyntaxKind::PATH_META
            | SyntaxKind::TOKEN_TREE_META
            | SyntaxKind::KEY_VALUE_META
            | SyntaxKind::VISIBILITY
    );
    (!elsewhere).then_some(head)
}

fn is_derive(meta: &ast::Meta) -> bool {
    let path = match meta {
        ast::Meta::TokenTreeMeta(meta) => meta.path(),
        _ => None,
    };
    path.and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "derive")
}

fn in_token_tree(token: &SyntaxToken) -> bool {
    token.parent().is_some_and(|it| it.kind() == SyntaxKind::TOKEN_TREE)
}

/// Whether `token` is followed or preceded by `::`, which a token tree has as one token or as
/// two `:` with nothing between.
fn by_colons(token: &SyntaxToken) -> bool {
    fn colons(first: Option<SyntaxToken>, step: fn(&SyntaxToken) -> Option<SyntaxToken>) -> bool {
        let Some(first) = first else { return false };
        match first.kind() {
            T![::] => true,
            T![:] => step(&first).is_some_and(|it| it.kind() == T![:]),
            _ => false,
        }
    }
    colons(token.next_token(), SyntaxToken::next_token)
        || colons(token.prev_token(), SyntaxToken::prev_token)
}

/// Whether a `#[cfg]` or `#![cfg]` of `node` is false, which leaves it out of the crate.
fn configured_out(node: &SyntaxNode, cfg: &cfg::CfgOptions) -> bool {
    let Some(owner) = ast::AnyHasAttrs::cast(node.clone()) else { return false };
    ast::attrs_including_inner(&owner).filter_map(|it| it.meta()).any(|meta| match meta {
        ast::Meta::CfgMeta(meta) => meta.cfg_predicate().is_some_and(|predicate| {
            cfg.check(&cfg::CfgExpr::parse_from_ast(predicate)) == Some(false)
        }),
        _ => false,
    })
}
