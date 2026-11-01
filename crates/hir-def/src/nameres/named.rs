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
//! The crate's files are read first, then the expansions its def map made (macro calls, attribute
//! macros and derives at the level of items, in traits and impls), then the bodies, each lowered
//! with the expansions it made, and its blocks' items'; each step only while a dependency is not
//! named yet: most crates name each in a `use`, and lowering a body is the costly part.

use base_db::{Crate, CrateOrigin, Db};
use blaze_hash::HashSet;
use either::Either;
use hir_expand::{HirFileId, MacroCallId};
use syntax::{
    NodeOrToken, SyntaxKind, T, WalkEvent,
    view::{self as ast, AstNode as _, Node, Token},
};

use crate::{
    ModuleDefId,
    expr_store::Body,
    item_scope::ItemScope,
    nameres::{
        DefMap,
        assoc::{ImplItems, TraitItems},
        crate_def_map,
        demand::{body_blocks, crate_body_owners},
    },
};

/// The dependencies of `krate` that its code names, as rustc loads them, with those of the
/// standard library's that it is given (rustc injects `std` or `core`, and blaze builds the
/// standard library as a whole): every one, for a crate of the standard library. In the order
/// `krate` lists its dependencies. A proc-macro crate is among them when it is named, though a
/// program does not link one.
#[blaze_query::tracked(returns(ref))]
pub fn named_dependencies(db: &Db, krate: Crate) -> Box<[Crate]> {
    let dependencies = &krate.data(db).dependencies;
    if matches!(krate.data(db).origin, CrateOrigin::Lang(_)) {
        // alloc: the memo's value, once per crate
        return dependencies.iter().map(|it| it.crate_id).collect();
    }
    // Names are unique among a crate's dependencies: each is its `--extern` name.
    let unnamed: HashSet<&str> = dependencies
        .iter()
        .filter(|it| !matches!(it.crate_id.data(db).origin, CrateOrigin::Lang(_)))
        .map(|it| it.name.as_str())
        // alloc: one set per crate's walk
        .collect();
    let mut walk = Walk {
        db,
        cfg: krate.cfg_options(db),
        named: HashSet::default(),
        unnamed,
        read: HashSet::default(),
    };
    walk.crate_(krate);
    dependencies
        .iter()
        .filter(|it| {
            matches!(it.crate_id.data(db).origin, CrateOrigin::Lang(_))
                || walk.named.contains(it.name.as_str())
        })
        .map(|it| it.crate_id)
        // alloc: the memo's value, once per crate
        .collect()
}

struct Walk<'db> {
    db: &'db Db,
    cfg: &'db cfg::CfgOptions,
    /// The dependencies not named yet, by the name the crate has for them.
    unnamed: HashSet<&'db str>,
    named: HashSet<&'db str>,
    /// The files and expansions read.
    read: HashSet<HirFileId>,
}

impl<'db> Walk<'db> {
    fn crate_(&mut self, krate: Crate) {
        let def_map = crate_def_map(self.db, krate);
        for (_, module) in def_map.modules(self.db) {
            self.file(module.definition_source_file_id());
        }
        self.expansions_of(def_map);
        let mut owners = crate_body_owners(self.db, krate);
        // alloc: one set per crate's walk
        let mut seen: HashSet<_> = owners.iter().copied().collect();
        owners.reverse();
        while !self.unnamed.is_empty()
            && let Some(owner) = owners.pop()
        {
            let source_map = Body::unmoved_source_map(self.db, owner);
            for (_, &call) in source_map.expansions() {
                self.expansion(call);
            }
            let blocks = body_blocks(self.db, owner);
            for &(_, def_map) in &blocks.blocks {
                self.expansions_of(def_map);
            }
            owners.extend(blocks.owners.iter().rev().copied().filter(|&it| seen.insert(it)));
        }
    }

    /// The expansions `def_map` made, outside bodies.
    fn expansions_of(&mut self, def_map: &'db DefMap) {
        for (_, module) in def_map.modules(self.db) {
            if self.unnamed.is_empty() {
                return;
            }
            self.scope(module.scope(self.db));
        }
    }

    fn scope(&mut self, scope: &'db ItemScope) {
        for (_, &call) in scope.iter_macro_invoc() {
            self.expansion(call);
        }
        for (_, call) in scope.attr_macro_invocs() {
            self.expansion(call);
        }
        for (_, derives) in scope.derive_macro_invocs() {
            for (_, _, calls) in derives {
                // A builtin derive has no expansion: what it writes names `core` alone.
                for &call in calls.iter().flatten() {
                    if let Either::Left(call) = call {
                        self.expansion(call);
                    }
                }
            }
        }
        for def in scope.declarations() {
            if let ModuleDefId::TraitId(it) = def {
                for (_, call) in TraitItems::query(self.db, it).macro_calls() {
                    self.expansion(call);
                }
            }
        }
        for it in scope.impls() {
            for (_, call) in ImplItems::of(self.db, it).0.macro_calls() {
                self.expansion(call);
            }
        }
    }

    fn expansion(&mut self, call: MacroCallId) {
        if !self.unnamed.is_empty() {
            self.file(call.into());
        }
    }

    fn file(&mut self, file: HirFileId) {
        if self.read.insert(file) {
            self.node(file.root(self.db));
        }
    }

    fn node(&mut self, root: Node<'db>) {
        let mut events = root.preorder_with_tokens();
        while let Some(event) = events.next() {
            if self.unnamed.is_empty() {
                return;
            }
            let WalkEvent::Enter(element) = event else { continue };
            let node = match element {
                NodeOrToken::Token(token) => {
                    if token.kind() == SyntaxKind::IDENT && in_token_tree(token) && by_colons(token)
                    {
                        self.name(token.text());
                    }
                    continue;
                }
                NodeOrToken::Node(node) => node,
            };
            if configured_out(node, self.cfg) {
                events.skip_subtree();
                continue;
            }
            if let Some(path) = ast::Path::cast(node) {
                if let Some(head) = looked_up(path) {
                    self.name(head.text());
                }
            } else if let Some(extern_crate) = ast::ExternCrate::cast(node)
                && let Some(name) = extern_crate.name_ref()
            {
                self.name(name.text());
            }
        }
    }

    fn name(&mut self, text: &str) {
        let text = text.strip_prefix("r#").unwrap_or(text);
        if let Some(name) = self.unnamed.take(text) {
            self.named.insert(name);
        }
    }
}

/// The first segment of `path` when it is one rustc may look up in the extern prelude.
fn looked_up(path: ast::Path<'_>) -> Option<ast::NameRef<'_>> {
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

fn in_token_tree(token: Token<'_>) -> bool {
    token.parent().is_some_and(|it| it.kind() == SyntaxKind::TOKEN_TREE)
}

/// Whether `token` is followed or preceded by `::`, which a token tree has as one token or as
/// two `:` with nothing between.
fn by_colons(token: Token<'_>) -> bool {
    fn colons<'t>(first: Option<Token<'t>>, step: fn(Token<'t>) -> Option<Token<'t>>) -> bool {
        let Some(first) = first else { return false };
        match first.kind() {
            T![::] => true,
            T![:] => step(first).is_some_and(|it| it.kind() == T![:]),
            _ => false,
        }
    }
    colons(token.next_token(), |it| it.next_token())
        || colons(token.prev_token(), |it| it.prev_token())
}

/// Whether a `#[cfg]` or `#![cfg]` of `node` is false, which leaves it out of the crate.
fn configured_out(node: Node<'_>, cfg: &cfg::CfgOptions) -> bool {
    let Some(owner) = ast::AnyHasAttrs::cast(node) else { return false };
    ast::attrs_including_inner(&owner).filter_map(|it| it.meta()).any(|meta| match meta {
        ast::Meta::CfgMeta(meta) => meta.cfg_predicate().is_some_and(|predicate| {
            cfg.check(&cfg::CfgExpr::parse_from_view(predicate)) == Some(false)
        }),
        _ => false,
    })
}
