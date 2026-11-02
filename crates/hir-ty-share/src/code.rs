//! The code of a body as the code generator compiles it: its item's code hash
//! ([`blaze_items`], each callee by its interface hash) with what a template built from it
//! holds that the code hash leaves out: the names of its bindings, which its debug info tells,
//! and the positions its MIR names, by span table and index ([`table_of`]). A template that
//! names an expression by its id is never reused once that id is another expression's.
//!
//! The code generator's `Hashes::code` is this, and a coroutine's identity
//! ([`crate::identity`]) holds it: its layout, its drop glue and the debug info of a value of
//! it read its body's locals and their names.

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, closure_code, code_hash, coroutine_code, interface_hash, key};
use hir_def::{DefWithBodyId, expr_store::ExpressionStore};
use hir_ty::{
    InferBodyId, InferenceResult,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    mir::{MirBody, MirSpan},
    next_solver::TyKind,
};
use rustc_type_ir::inherent::IntoKind;

use crate::{Unshared, identity::unhashable};

pub const CODE_DOMAIN: &str = "blaze v2 codegen code 1";
pub const DISCRIMINANT_DOMAIN: &str = "blaze v2 codegen discriminant 1";
pub const NAMES_DOMAIN: &str = "blaze v2 codegen binding names 1";

/// The tables of a body's spans, by the kind of span: [`MirSpan`]'s variants.
pub const EXPRS: usize = 0;
pub const EXPR_ENDS: usize = 1;
pub const PATS: usize = 2;
pub const BINDINGS: usize = 3;
pub const SELF_PARAM: usize = 4;
pub const UNKNOWN: usize = 5;
pub const TABLES: usize = 6;

/// What a body's code is beside its code hash, in the preimage of [`CODE_DOMAIN`].
const ITEM: u8 = 0;
const CLOSURE: u8 = 1;
const COROUTINE: u8 = 2;
const BY_MOVE: u8 = 3;

/// Which table of its owner's a span is in, and its index there; for a span of code inlined
/// into the owner's body, which is of another's, its index in `Inlined::spans`.
pub fn table_of(span: MirSpan) -> Result<(usize, u32), u32> {
    Ok(match span {
        MirSpan::ExprId(it) => (EXPRS, it.into_raw().into_u32()),
        MirSpan::ExprEnd(it) => (EXPR_ENDS, it.into_raw().into_u32()),
        MirSpan::PatId(it) => (PATS, it.into_raw().into_u32()),
        MirSpan::BindingId(it) => (BINDINGS, it.into_raw().into_u32()),
        MirSpan::SelfParam => (SELF_PARAM, 0),
        MirSpan::Unknown => (UNKNOWN, 0),
        MirSpan::Inlined(at) => return Err(at),
    })
}

/// The code hash beside what a template built from `body` holds of it that the code hash
/// leaves out: the names of its bindings and the positions it names, by table and index, in
/// the order of its blocks.
fn with_spans(db: &Db, kind: u8, parts: &[ContentId], body: &MirBody<'_>) -> ContentId {
    // alloc: one buffer per body, this query's miss, memoized after
    let mut out = Vec::with_capacity(1 + 32 * (parts.len() + 1) + 5 * 8 * body.basic_blocks.len());
    out.push(kind);
    for part in parts {
        out.extend_from_slice(&part.0);
    }
    out.extend_from_slice(&names(db, body).0);
    let push = |out: &mut Vec<u8>, span: Option<MirSpan>| match span.map(table_of) {
        Some(Ok((table, index))) => {
            out.push(table as u8);
            out.extend_from_slice(&index.to_le_bytes());
        }
        Some(Err(at)) => {
            out.push(0xfe);
            out.extend_from_slice(&at.to_le_bytes());
        }
        None => out.push(0xff),
    };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            push(&mut out, Some(statement.span));
            statement.kind.for_each_operand(&mut |it| push(&mut out, it.span));
        }
        if let Some(terminator) = &block.terminator {
            push(&mut out, Some(terminator.span));
            terminator.kind.for_each_operand(&mut |it| push(&mut out, it.span));
        }
    }
    // The spans of inlined code, each of the body of its scope's callee, and where each
    // scope was inlined at.
    let inlined = &body.inlined;
    out.extend_from_slice(&(inlined.spans.len() as u32).to_le_bytes());
    for span in &inlined.spans {
        out.extend_from_slice(&span.scope.to_le_bytes());
        push(&mut out, Some(span.span));
    }
    out.extend_from_slice(&(inlined.scopes.len() as u32).to_le_bytes());
    for scope in &inlined.scopes {
        out.extend_from_slice(&scope.callee.to_le_bytes());
        push(&mut out, Some(scope.call_site));
    }
    ContentId::of(CODE_DOMAIN, &out)
}

/// The name of each of `body`'s bindings, in the order of their locals, each framed by its
/// local and its length.
fn names(db: &Db, body: &MirBody<'_>) -> ContentId {
    let store = ExpressionStore::of(db, body.owner.expression_store_owner(db));
    let mut out = Hasher::new(NAMES_DOMAIN);
    for (local, binding) in body.local_to_binding_map().iter() {
        let name = store[*binding].name.as_str();
        out.update(&local.into_raw().into_u32().to_le_bytes());
        out.update(&(name.len() as u32).to_le_bytes());
        out.update(name.as_bytes());
    }
    out.finish()
}

fn no_mir(what: impl std::fmt::Debug, error: impl std::fmt::Debug) -> Unshared {
    Unshared::NoMir(format!("the MIR of {what:?} failed to lower: {error:?}")) // alloc: error path
}

/// The code of a function, constant or static: its item's code hash. An enum variant's
/// discriminant is a body of its enum's interface.
#[blaze_query::tracked(returns(ref))]
pub fn of_item(db: &Db, def: DefWithBodyId) -> Result<ContentId, Unshared> {
    let item = ItemId::of_body(db, def);
    let code = match def {
        DefWithBodyId::VariantId(variant) => {
            let mut h = Hasher::new(DISCRIMINANT_DOMAIN);
            h.id(&key::variant_key(db, variant).0)
                .id(&interface_hash(db, item).map_err(unhashable(db))?.0);
            h.finish()
        }
        DefWithBodyId::FunctionId(_) | DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) => {
            match code_hash(db, item).map_err(unhashable(db))? {
                Some(code) => code.0,
                None => {
                    return Err(Unshared::NoMir(format!( // alloc: error path
                        "blaze-items: {:?} `{}` has no body to compile",
                        item.kind(),
                        key::path(db, item)
                    )));
                }
            }
        }
    };
    let body = db.mir_body(InferBodyId::DefWithBodyId(def)).map_err(|it| no_mir(def, it))?;
    Ok(with_spans(db, ITEM, &[code], body))
}

#[blaze_query::tracked(returns(ref))]
pub fn of_closure<'db>(db: &'db Db, closure: InternedClosureId<'db>) -> Result<ContentId, Unshared> {
    let code = closure_code(db, closure).map_err(unhashable(db))?;
    let body = db.mir_body_for_closure(closure).map_err(|it| no_mir(closure, it))?;
    Ok(with_spans(db, CLOSURE, &[code.0], body))
}

#[blaze_query::tracked(returns(ref))]
pub fn of_coroutine<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, Unshared> {
    let code = coroutine_code(db, coroutine).map_err(unhashable(db))?;
    let body = db.mir_body_for_coroutine(coroutine).map_err(|it| no_mir(coroutine, it))?;
    Ok(with_spans(db, COROUTINE, &[code.0], body))
}

/// The future an `async` closure gives when called by value: the coroutine's body lowered
/// against the closure's captures, so its code is the coroutine's with what the closure
/// captures and how.
#[blaze_query::tracked(returns(ref))]
pub fn of_coroutine_by_move<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, Unshared> {
    let code = coroutine_code(db, coroutine).map_err(unhashable(db))?;
    let loc = coroutine.loc(db);
    let closure = ExpressionStore::closure_for_coroutine(loc.expr);
    let closure = match InferenceResult::of(db, loc.owner).expr_ty(closure).kind() {
        TyKind::CoroutineClosure(id, _) => id.0.as_closure(db),
        other => {
            return Err(Unshared::NoMir(format!( // alloc: error path
                "the coroutine {coroutine:?}, run by move, is of no `async` closure but of {other:?}"
            )));
        }
    };
    let captures =
        blaze_items::value(db, &blaze_items::captures(db, closure)).map_err(unhashable(db))?;
    let body =
        db.mir_body_for_coroutine_by_move(coroutine).map_err(|it| no_mir(coroutine, it))?;
    Ok(with_spans(db, BY_MOVE, &[code.0, captures], body))
}
