//! `lock_across_query`: a lock guard live across a call of a query (AGENTS.md rule 9: a lock is
//! held only for a field update, never across a query call), whether or not the query parks the
//! fiber: a query may run long or wait itself, and every task that wants the lock queues behind
//! it, the convoy rule 9 bans.
//!
//! A guard is `parking_lot`'s (`lock_api`'s `MutexGuard`, `RwLock` read, write and upgradable
//! guards, the mapped, `Arc` and reentrant ones) or std's (`MutexGuard`, `RwLock`'s guards, the
//! mapped ones, `ReentrantLockGuard`), itself or held in a tuple, an array or a field. A query is
//! a function an attribute macro of `blaze_query_macros` made (`#[blaze_query::tracked]`,
//! `input`, `interned`: the macro the attribute resolves to, read off the function's own
//! definition), or a method of a trait implemented for `base_db::Db` (`HirDatabase`'s).
//!
//! What is live across a call is the MIR's, as the may-suspend check reads it
//! ([`hir_ty_mir::mir::MirBody::held_across_calls_in`] over the runtime body, the body's
//! closures included): a guard dropped before the call is not held, however its scope is
//! written.

use base_db::Db;
use blaze_hash::HashMap;
use hir::{AsAssocItem as _, HirDisplay as _, HirFileId};
use hir_def::{
    DefWithBodyId, FunctionId,
    hir::{Expr, ExprId},
    signatures::FunctionSignature,
};
use hir_expand::{MacroDefKind, proc_macro::ProcMacroKind};
use hir_ty::{InferBodyId, db::HirDatabase as _, next_solver::GenericArgs};
use hir_ty_mir::mir::{BasicBlockId, MirBody, MirSpan, TerminatorKind};
use rustc_type_ir::inherent::GenericArgs as _;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts::{self, HOLDS_DEPTH},
};
use crate::check::Cx;

/// The attribute macros of `blaze_query_macros` whose functions are queries.
const QUERY_MACROS: &[&str] = &["tracked", "input", "interned"];

/// The guards of `lock_api` (whose types `parking_lot`'s are) and of std, by crate.
const GUARDS: &[(&str, &[&str])] = &[
    (
        "lock_api",
        &[
            "MutexGuard",
            "MappedMutexGuard",
            "ArcMutexGuard",
            "RwLockReadGuard",
            "RwLockWriteGuard",
            "RwLockUpgradableReadGuard",
            "MappedRwLockReadGuard",
            "MappedRwLockWriteGuard",
            "ArcRwLockReadGuard",
            "ArcRwLockWriteGuard",
            "ArcRwLockUpgradableReadGuard",
            "ReentrantMutexGuard",
            "MappedReentrantMutexGuard",
            "ArcReentrantMutexGuard",
        ],
    ),
    (
        "std",
        &[
            "MutexGuard",
            "MappedMutexGuard",
            "RwLockReadGuard",
            "RwLockWriteGuard",
            "MappedRwLockReadGuard",
            "MappedRwLockWriteGuard",
            "ReentrantLockGuard",
        ],
    ),
];

/// How many bodies (the body and its closures, nested) one body's walk reads at most.
const MOST_BODIES: usize = 256;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::LockAcrossQuery) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// What makes a callee a query, as a finding says it.
#[derive(Clone)]
enum Query {
    /// Made by this attribute macro of `blaze_query_macros`.
    Macro(&'static str),
    /// A method of this trait, implemented for `base_db::Db`.
    Database(String),
}

impl Query {
    fn say(&self) -> String {
        match self {
            Query::Macro(name) => format!("made by `#[blaze_query::{name}]`"),
            Query::Database(name) => format!("a method of `{name}` on the database"),
        }
    }
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    // alloc: the query calls of one body, a few; each callee asked once
    let mut queries: Vec<(ExprId, FunctionId, Query)> = Vec::new();
    // alloc: as above
    let mut known: HashMap<FunctionId, Option<Query>> = HashMap::default();
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((f, args)) = written_callee(infer, body, id) else { continue };
        let query = known.entry(f).or_insert_with(|| query_of(db, analysis, f, args));
        if let Some(query) = query {
            queries.push((id, f, query.clone()));
        }
    }
    if queries.is_empty() {
        return found;
    }
    let Ok(root) = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)) else { return found };
    // alloc: the body's MIR and its closures', a few
    let mut bodies: Vec<&MirBody<'_>> = vec![root];
    let mut at = 0;
    while at < bodies.len() && bodies.len() < MOST_BODIES {
        let mir = bodies[at];
        for &closure in &mir.closures {
            if let Ok(it) = db.runtime_mir_body_for_closure(closure) {
                bodies.push(it);
            }
        }
        at += 1;
    }
    let target = cx.krate().to_display_target(db);
    for mir in bodies {
        // alloc: the query calls of one MIR body, a few
        let mut blocks: Vec<BasicBlockId> = Vec::new();
        // alloc: as above
        let mut calls: Vec<(ExprId, FunctionId, &Query)> = Vec::new();
        for (block, data) in mir.basic_blocks.iter() {
            let Some(terminator) = &data.terminator else { continue };
            let (TerminatorKind::Call { .. }, MirSpan::ExprId(expr)) = (&terminator.kind, terminator.span) else {
                continue;
            };
            let Some((_, f, query)) = queries.iter().find(|(it, ..)| *it == expr) else { continue };
            blocks.push(block);
            calls.push((expr, *f, query));
        }
        if blocks.is_empty() {
            continue;
        }
        blaze_mem::frame::task(blaze_mem::Tag::Diagnostics, |frame| {
            let across = mir.held_across_calls_in(frame, &blocks);
            for (&(expr, f, query), locals) in calls.iter().zip(across.iter()) {
                // alloc: the guards one finding names, one or two
                let mut guards: Vec<String> = Vec::new();
                for &local in locals.iter() {
                    let ty = analysis.ty(mir.locals[local].ty.as_ref());
                    if ty.is_reference() || facts::holds(db, &ty, HOLDS_DEPTH, &mut |adt| is_guard(db, adt)).is_none() {
                        continue;
                    }
                    let binding = mir.binding_locals.iter().find_map(|(binding, &it)| (it == local).then_some(binding));
                    let ty = ty.display(db, target);
                    let said = match binding {
                        Some(binding) => format!("`{}` (`{ty}`)", body[binding].name.as_str()),
                        None => format!("a temporary `{ty}`"),
                    };
                    if !guards.contains(&said) {
                        guards.push(said);
                    }
                }
                let Some(first) = guards.first() else { continue };
                let Some(range) = analysis.expr_range(expr) else { continue };
                let name = FunctionSignature::of(db, f).name.as_str();
                let message = match &*guards {
                    [_] => format!("the lock guard {first} is held across the query `{name}` ({})", query.say()),
                    _ => format!(
                        "the lock guards {} are held across the query `{name}` ({})",
                        guards.join(", "),
                        query.say()
                    ),
                };
                let note = "a lock is held for a field update, never across a query call (AGENTS.md rule 9): the query \
                            may run long or wait, and every task that wants the lock queues behind it meanwhile";
                facts::push(&mut found, Lint::LockAcrossQuery, range, message, Some(note.to_owned()));
            }
        });
    }
    found
}

fn is_guard(db: &Db, adt: hir::Adt) -> bool {
    let name = adt.name(db);
    let krate = adt.module(db).krate(db);
    GUARDS.iter().any(|(it, names)| names.contains(&name.as_str()) && facts::crate_is(db, krate, it))
}

/// Whether a call of `f` with `args` is a query, and what makes it one.
fn query_of<'db>(db: &'db Db, analysis: &hir::BodyAnalysis<'db>, f: FunctionId, args: GenericArgs<'db>) -> Option<Query> {
    if let Some(name) = query_macro(db, f) {
        return Some(Query::Macro(name));
    }
    let item = hir::Function::from(f).as_assoc_item(db)?;
    let (trait_, self_ty) = match item.container(db) {
        hir::AssocItemContainer::Trait(trait_) => (trait_, analysis.ty(args.type_at(0))),
        hir::AssocItemContainer::Impl(imp) => (imp.trait_(db)?, imp.self_ty(db)),
    };
    let on_db = self_ty.strip_references().as_adt().is_some_and(|it| facts::adt_is(db, it, "base_db", "Db"));
    on_db.then(|| Query::Database(trait_.name(db).as_str().to_owned()))
}

/// The attribute macro of `blaze_query_macros` that made `f`, if one of [`QUERY_MACROS`] did:
/// the macro of the call whose expansion `f` is defined in.
fn query_macro(db: &Db, f: FunctionId) -> Option<&'static str> {
    let HirFileId::MacroFile(call) = DefWithBodyId::from(f).erased_ast_id(db).file_id else { return None };
    let loc = call.loc(db);
    let &MacroDefKind::ProcMacro(id, _, ProcMacroKind::Attr) = &loc.def.kind else { return None };
    if !facts::crate_is(db, hir::Crate::from(loc.def.krate), "blaze_query_macros") {
        return None;
    }
    let HirFileId::FileId(file) = id.file_id else { return None };
    let ptr = id.file_id.ast_id_map(db).try_get_erased(id.value.erase())?;
    let defined = ast::Fn::cast(ast::try_to_node(ptr, file.parse(db).root())?)?;
    let name = defined.name()?;
    QUERY_MACROS.iter().find(|&&it| it == name.text()).copied()
}
