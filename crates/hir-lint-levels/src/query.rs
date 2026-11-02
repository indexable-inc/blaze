//! The level of a lint at an item, or at an expression of its store, as front-end queries:
//! rustc's `shallow_lint_levels_on` and `lint_level_at_node`, for what must know a level to
//! decide what a program does (the const evaluator's `long_running_const_eval`).
//!
//! Cost: a level asked again is a memo probe and one bit test of the flattened set for a lint
//! nothing names, else a scan of the 2-byte ids of the few named from the item out. The first
//! one walks the item's node out to the crate root, reading the attributes of each node that
//! has any, and keeps two short slices.

use base_db::{Crate, Db};
use blaze_lint_defs::{Level, LintId, LintSet};
use hir_def::{
    AdtId, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, HasModule, Lookup, VariantId,
    expr_store::ExpressionStore, hir::ExprId, src::HasSource,
};
use hir_expand::InFile;
use span::Edition;
use syntax::{AstPtr, SyntaxNodePtr, view as ast};

use crate::{
    attrs,
    builder::{LevelCx, Levels, SetId, SourceId, Spec, reveal},
};

/// The levels in force at one node, flattened: each lint the command line or an attribute from
/// the crate root to the node names, at the innermost level, before `warnings` and the cap,
/// which [`ScopeLevels::level`] applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeLevels {
    /// The lints of `ids`: one bit test says a lint is at its default level.
    named: LintSet,
    ids: Box<[LintId]>,
    levels: Box<[Level]>,
    edition: Edition,
    cap: Option<Level>,
}

impl ScopeLevels {
    fn of(cx: &LevelCx<'_, '_>, levels: &Levels<'_>, set: SetId) -> ScopeLevels {
        let (named, flat) = levels.flatten(set);
        ScopeLevels {
            named,
            // alloc: once per query result, the few lints named from the owner out
            ids: flat.iter().map(|&(id, _)| id).collect(),
            // alloc: as `ids`
            levels: flat.iter().map(|&(_, level)| level).collect(),
            edition: cx.edition,
            cap: cx.opts.cap,
        }
    }

    fn raw(&self, id: LintId) -> Option<Spec> {
        if !self.named.contains(id) {
            return None;
        }
        let at = self.ids.binary_search(&id).expect("a lint of `named` is in `ids`");
        Some(Spec { level: self.levels[at], expect: None, source: SourceId::DEFAULT })
    }

    /// The level of `lint` here: rustc's `reveal_actual_level`, with `warnings` and the cap.
    pub fn level(&self, lint: LintId) -> Level {
        reveal(lint.lint(), self.raw(lint), self.raw(LintId::WARNINGS), self.edition, self.cap).level
    }
}

/// rustc's `shallow_lint_levels_on` of `owner`'s item, flattened (see [`ScopeLevels`]): the
/// command line the crate is given (base-db's `Crate::lint_options`) and the attributes of the
/// item and of what is around it, through the macro calls it is made by and the `mod`s it is
/// in, as the HIR has them: the innermost wins, and a `forbid` does not hold against one.
///
/// The `()` is there because the query engine needs it: a tracked function of one argument is
/// keyed by that argument, an interned or input struct or a `blaze_query::Supertype` enum, and
/// `ExpressionStoreOwnerId` can be neither (its variants overlap: `Body(FunctionId)` and
/// `Signature(FunctionId)` are one id). With two arguments the engine interns the pair into the
/// function's hidden key (as hir-ty's `closure_exprs`).
#[blaze_query::tracked(returns(ref))]
pub fn lint_levels_on(db: &Db, owner: ExpressionStoreOwnerId, _: ()) -> ScopeLevels {
    let krate = owner.krate(db);
    let mut cx = LevelCx::new(db, krate, krate.lint_options(db));
    let mut levels = Levels::new(&mut cx, &mut (), false, false);
    let set = levels.set_at(&mut cx, &mut (), owner_node(db, owner));
    ScopeLevels::of(&cx, &levels, set)
}

/// The levels at `krate`'s root: what is in force where no item of the crate is, as rustc's
/// `CRATE_HIR_ID` has them.
#[blaze_query::tracked(returns(ref))]
pub fn crate_lint_levels(db: &Db, krate: Crate) -> ScopeLevels {
    let mut cx = LevelCx::new(db, krate, krate.lint_options(db));
    let mut levels = Levels::new(&mut cx, &mut (), false, false);
    let root = InFile::new(cx.root, cx.root.root(db));
    let set = levels.set_at(&mut cx, &mut (), root);
    ScopeLevels::of(&cx, &levels, set)
}

/// The level of `lint` at `owner`'s item.
pub fn lint_level(db: &Db, owner: DefWithBodyId, lint: LintId) -> Level {
    lint_levels_on(db, ExpressionStoreOwnerId::Body(owner), ()).level(lint)
}

/// The level of `lint` at the crate root of `krate`: for an evaluation with no frame of a body
/// of the crate, as rustc's `best_lint_scope` falls to `CRATE_HIR_ID`.
pub fn crate_lint_level(db: &Db, krate: Crate, lint: LintId) -> Level {
    crate_lint_levels(db, krate).level(lint)
}

/// What stops [`lint_level_at`]: a level it can't find without guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelError {
    /// `expr` of `owner`'s store is desugared and has no syntax, so the statements and
    /// expressions with attributes between it and the item can't be found. The caller asks at
    /// the nearest expression of its scope that has syntax (rustc's lint root of a desugared
    /// expression is the scope it is built in).
    NoSyntax { owner: ExpressionStoreOwnerId, expr: ExprId },
}

impl std::fmt::Display for LevelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LevelError::NoSyntax { owner, expr } => {
                write!(f, "no lint level at {expr:?} of {owner:?}: the expression is desugared and has no syntax")
            }
        }
    }
}

impl std::error::Error for LevelError {}

/// The level of `lint` at `expr` of `owner`'s expression store, or at the item for `None`:
/// rustc's `lint_level_at_node` at a MIR source scope's lint root. An anon const is its
/// parent's store at its root expression; an inline const is an expression of its body.
///
/// The statements and expressions from `expr` out to the item are read for attributes; when
/// none has any (nearly always), this is [`lint_levels_on`] of the item. A desugared `expr`,
/// which has no syntax, is [`LevelError::NoSyntax`], never the item's level: an attribute
/// between them would be missed.
pub fn lint_level_at(db: &Db, owner: ExpressionStoreOwnerId, expr: Option<ExprId>, lint: LintId) -> Result<Level, LevelError> {
    let Some(expr) = expr else { return Ok(lint_levels_on(db, owner, ()).level(lint)) };
    let Some(node) = expr_node(db, owner, expr) else { return Err(LevelError::NoSyntax { owner, expr }) };
    let krate = owner.krate(db);
    let item = attrs::key(owner_node(db, owner));
    let cfg = krate.cfg_options(db);
    let edition = krate.data(db).edition;
    let mut at = Some(node);
    let mut attributed = false;
    while let Some(here) = at {
        if attrs::key(here) == item {
            break;
        }
        if attrs::owner(here.value, false).is_some_and(|it| !attrs::read(it, cfg, edition).is_empty()) {
            attributed = true;
            break;
        }
        at = attrs::parent(db, krate, here);
    }
    if !attributed {
        return Ok(lint_levels_on(db, owner, ()).level(lint));
    }
    let mut cx = LevelCx::new(db, krate, krate.lint_options(db));
    let mut levels = Levels::new(&mut cx, &mut (), false, false);
    let set = levels.set_at(&mut cx, &mut (), node);
    Ok(levels.spec(&cx, lint.lint(), set).level)
}

/// The node of `expr` of `owner`'s store, none for a desugared one.
fn expr_node(db: &Db, owner: ExpressionStoreOwnerId, expr: ExprId) -> Option<InFile<ast::Node<'_>>> {
    let (_, map) = ExpressionStore::with_source_map(db, owner);
    let source = map.expr_syntax(expr).ok()?;
    let ptr = SyntaxNodePtr::from(source.value);
    Some(InFile::new(source.file_id, ast::to_node(ptr, source.file_id.root(db))))
}

/// The item node of `owner`: the function, const, static, enum variant, ADT, trait, impl or
/// type alias whose body, signature or fields the store is.
pub fn owner_node(db: &Db, owner: ExpressionStoreOwnerId) -> InFile<ast::Node<'_>> {
    match owner {
        ExpressionStoreOwnerId::Body(body) => match body {
            DefWithBodyId::FunctionId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            DefWithBodyId::StaticId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            DefWithBodyId::ConstId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            DefWithBodyId::VariantId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
        },
        ExpressionStoreOwnerId::Signature(def) => match def {
            GenericDefId::AdtId(AdtId::StructId(id)) => node_of(db, id.lookup(db).ast_ptr(db)),
            GenericDefId::AdtId(AdtId::UnionId(id)) => node_of(db, id.lookup(db).ast_ptr(db)),
            GenericDefId::AdtId(AdtId::EnumId(id)) => node_of(db, id.lookup(db).ast_ptr(db)),
            GenericDefId::ConstId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            GenericDefId::FunctionId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            GenericDefId::ImplId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            GenericDefId::StaticId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            GenericDefId::TraitId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            GenericDefId::TypeAliasId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
        },
        ExpressionStoreOwnerId::VariantFields(variant) => match variant {
            VariantId::StructId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            VariantId::UnionId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
            VariantId::EnumVariantId(id) => node_of(db, id.lookup(db).ast_ptr(db)),
        },
    }
}

fn node_of<N: syntax::AstNode>(db: &Db, ptr: InFile<AstPtr<N>>) -> InFile<ast::Node<'_>> {
    InFile::new(ptr.file_id, ast::to_node(ptr.value.syntax_node_ptr(), ptr.file_id.root(db)))
}
