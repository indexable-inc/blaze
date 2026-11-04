//! The code of a body as the code generator compiles it: its item's code hash
//! ([`blaze_items`], each callee by its interface hash) with what a template built from it
//! holds that the code hash leaves out: the positions its MIR names, by span table and index
//! ([`table_of`]). A template that names an expression by its id is never reused once that id
//! is another expression's.
//!
//! No name is in it: not its item's, not a callee's, not a binding's
//! (docs/v2/plan/design/name-free-identity.md). Its debug info names its bindings by index and
//! reads their names from the database at link (`hir-ty-codegen`'s `v2::side`), so renaming
//! a local keeps the code. What a compiled body does read of names is keyed beside this where
//! it is read: [`told_names`] for the types its debug info tells, [`coroutine_names`] for a
//! coroutine's type, whose debug info names the locals it keeps.
//!
//! The code generator's `Hashes::code` is this, and a coroutine's identity
//! ([`crate::identity`]) holds it with [`coroutine_names`]: its layout, its drop glue and the
//! debug info of a value of it read its body's locals and their names.

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, closure_code, code_hash, coroutine_code, interface_hash, item_key, key};
use hir_def::{CallableDefId, DefWithBodyId, HasModule as _, expr_store::ExpressionStore};
use hir_ty::{
    InferBodyId, InferenceResult,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    mir::{MirBody, MirSpan, is_by_move_coroutine},
    next_solver::{TermId, TyKind},
};
use rustc_type_ir::{
    ExistentialPredicate, TypeVisitableExt as _,
    inherent::{GenericArgs as _, IntoKind},
};

use crate::{Unshared, identity::unhashable};

pub const CODE_DOMAIN: &str = "blaze v2 body code 2";
pub const DISCRIMINANT_DOMAIN: &str = "blaze v2 codegen discriminant 1";
pub const NAMES_DOMAIN: &str = "blaze v2 codegen binding names 1";
pub const TOLD_DOMAIN: &str = "blaze v2 codegen told types 3";

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
/// leaves out: the positions it names, by table and index, in the order of its blocks.
fn with_spans(kind: u8, parts: &[ContentId], body: &MirBody<'_>) -> ContentId {
    // alloc: one buffer per body, this query's miss, memoized after
    let mut out = Vec::with_capacity(1 + 32 * parts.len() + 5 * 8 * body.basic_blocks.len());
    out.push(kind);
    for part in parts {
        out.extend_from_slice(&part.0);
    }
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
/// local and its length: what the debug info of a coroutine's value reads of its body
/// ([`coroutine_names`]).
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

/// The names of the bindings of a coroutine's body, run by move (`by_move`, an `async`
/// closure's future called by value) or not: what its type's identity holds beside its code,
/// as the debug info of a value of it names the locals it keeps across its yields.
pub fn coroutine_names<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
    by_move: bool,
) -> Result<ContentId, Unshared> {
    let body = match by_move {
        true => db.mir_body_for_coroutine_by_move(coroutine),
        false => db.mir_body_for_coroutine(coroutine),
    };
    Ok(names(db, body.map_err(|it| no_mir(coroutine, it))?))
}

/// What the debug info of `body`'s named locals reads of the types they are of, beside its
/// code: `hir-ty-codegen`'s `side::Told::ty` is the identity ([`crate::identity::ty`]) of
/// each local's type as its instance has it, and this is that identity spelled of the
/// body's own types, arm for arm, up to what the instance's arguments fill (a parameter,
/// keyed by the instance's arguments beside this): an ADT by its identity
/// ([`crate::identity::adt`]: its key, interface, layout and the `Drop` impls its fields
/// reach), a foreign type, function item, closure or trait object by its item keys, and a
/// coroutine by its key and the names of the locals it keeps ([`coroutine_names`]). Each
/// binding by its index, in order, with its type's parts in the order its walk meets them.
/// `None` when a told local's type holds an alias (a projection, an
/// opaque type, a free alias, an unevaluated constant): what it normalizes to is chosen by
/// an impl or a defining body neither this body's code nor the instance's arguments hold, so
/// the body's template is kept under its code, names and all.
pub fn told_names(db: &Db, body: &MirBody<'_>) -> Result<Option<ContentId>, Unshared> {
    let mut out = Hasher::new(TOLD_DOMAIN);
    let mut bindings = 0u64;
    // alloc: one local's type's parts at once, reused for each
    let mut parts: Vec<(u8, ContentId)> = Vec::new();
    // Each binding by its index, in order, with its own local's type: which binding is of
    // which type is what the side table tells (`side::Told { binding, ty }`).
    for (binding, &local) in body.binding_locals.iter() {
        parts.clear();
        let ty = body.locals[local].ty.as_ref();
        if ty.has_aliases() {
            return Ok(None);
        }
        for arg in ty.walk() {
            let Some(ty) = arg.ty() else { continue };
            match ty.kind() {
                TyKind::Adt(def, _) => {
                    parts.push((
                        0,
                        *crate::identity::adt(db, def.def_id()).as_ref().map_err(Clone::clone)?,
                    ));
                }
                TyKind::Foreign(it) => parts.push((1, item_key(db, ItemId::Alias(it.0)).0)),
                TyKind::FnDef(def, _) => parts.push((
                    2,
                    match def.0 {
                        CallableDefId::FunctionId(it) => item_key(db, ItemId::Function(it)),
                        CallableDefId::StructId(it) => item_key(db, ItemId::Adt(it.into())),
                        CallableDefId::EnumVariantId(it) => key::variant_key(db, it),
                    }
                    .0,
                )),
                TyKind::Closure(id, _) => parts.push((3, key::closure_key(db, id.0).0)),
                TyKind::CoroutineClosure(id, _) => {
                    parts.push((4, key::coroutine_closure_key(db, id.0).0))
                }
                TyKind::Coroutine(id, args) => {
                    let by_move = is_by_move_coroutine(db, id.0, args.as_coroutine());
                    parts.push((5, key::coroutine_key(db, id.0).0));
                    parts.push((6, coroutine_names(db, id.0, by_move)?));
                }
                TyKind::CoroutineWitness(id, _) => {
                    parts.push((5, key::coroutine_key(db, id.0).0));
                    parts.push((6, coroutine_names(db, id.0, false)?));
                }
                TyKind::Dynamic(predicates, _) => {
                    for predicate in predicates.iter() {
                        let key = match predicate.skip_binder() {
                            ExistentialPredicate::Trait(it) => {
                                item_key(db, ItemId::Trait(it.def_id.0))
                            }
                            ExistentialPredicate::AutoTrait(it) => {
                                item_key(db, ItemId::Trait(it.0))
                            }
                            ExistentialPredicate::Projection(it) => match it.def_id.0 {
                                TermId::TypeAliasId(alias) => item_key(db, ItemId::Alias(alias)),
                                TermId::ConstId(konst) => item_key(db, ItemId::Const(konst)),
                                TermId::RpititId(it) => item_key(db, ItemId::Function(it.function)),
                            },
                        };
                        parts.push((7, key.0));
                    }
                }
                // No item and no name: a primitive, a pointer, a reference, a slice, an array,
                // a tuple, a function pointer (their parts are walked), or a parameter the
                // instance's arguments fill. An alias was refused above.
                _ => {}
            }
        }
        // The parts in the order the type's walk meets them, as its identity spells them.
        bindings += 1;
        out.update(&binding.into_raw().into_u32().to_le_bytes());
        out.update(&(parts.len() as u64).to_le_bytes());
        for (kind, it) in &parts {
            out.update(&[*kind]).id(it);
        }
    }
    out.update(&bindings.to_le_bytes());
    Ok(Some(out.finish()))
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
                    return Err(Unshared::NoMir(format!(
                        // alloc: error path
                        "blaze-items: {:?} `{}` has no body to compile",
                        item.kind(),
                        key::path(db, item)
                    )));
                }
            }
        }
    };
    let body = db.mir_body(InferBodyId::DefWithBodyId(def)).map_err(|it| no_mir(def, it))?;
    Ok(with_spans(ITEM, &[code], body))
}

#[blaze_query::tracked(returns(ref))]
pub fn of_closure<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Result<ContentId, Unshared> {
    let code = closure_code(db, closure).map_err(unhashable(db))?;
    let body = db.mir_body_for_closure(closure).map_err(|it| no_mir(closure, it))?;
    Ok(with_spans(CLOSURE, &[code.0], body))
}

#[blaze_query::tracked(returns(ref))]
pub fn of_coroutine<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, Unshared> {
    let code = coroutine_code(db, coroutine).map_err(unhashable(db))?;
    let body = db.mir_body_for_coroutine(coroutine).map_err(|it| no_mir(coroutine, it))?;
    Ok(with_spans(COROUTINE, &[code.0], body))
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
            return Err(Unshared::NoMir(format!(
                // alloc: error path
                "the coroutine {coroutine:?}, run by move, is of no `async` closure but of {other:?}"
            )));
        }
    };
    // A closure's captures are its own body's, the same for every crate that asks: its own.
    let captures = blaze_items::value(db, loc.owner.krate(db), &blaze_items::captures(db, closure))
        .map_err(unhashable(db))?;
    let body = db.mir_body_for_coroutine_by_move(coroutine).map_err(|it| no_mir(coroutine, it))?;
    Ok(with_spans(BY_MOVE, &[code.0, captures], body))
}
