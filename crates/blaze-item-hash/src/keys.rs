//! The item key of a definition a stored record names, and back: what
//! `hir_ty::body_store` writes in place of each `SolverDefId` of an inference result, a MIR
//! body or a borrow check, so that a record written by one database is read by another.
//!
//! A key is [`blaze_items::key::def_key`]'s, like rustc's `DefPathHash`: the same definition
//! has the same key in every database. [`def_of`] goes back through
//! [`blaze_items::key::resolve`]. An anonymous constant's key resolves to its store and root
//! expression, but its id is interned with its type, which only the lowering of its owner
//! gives: `hir_ty::body_store` writes one by its location and type (`IdKind::AnonConst`), never
//! by key, so it and a closure written inside one (whose owner it is) give `None` here.
//!
//! A key is relative to the body whose record names it (docs/v2/plan/design/item-hash.md,
//! section 3.7) only for what is the body's own: its item, its closures, coroutines and
//! `impl Trait` return types are "self" (their place in the body's own text), so that a record
//! of a renamed body is read back. A relative key is [`RELATIVE`], a tag, an index and, for a
//! closure, its kind (`hir_ty::body_store::closure_kind_code`), which no hash is; reading one
//! back opens no expression store and lowers no body.
//!
//! Any other item, one of the body's paths' targets among them, is named by its plain item
//! key (docs/v2/plan/design/restore.md, "One form, a function: the end state"): a key that
//! was "the target of path `i` of the body's resolutions" needed the body lowered and
//! resolved to be read back. The price: a callee renamed misses its callers' records (the
//! name they looked up changed too).

use base_db::Db;
use blaze_ids::ContentId;
use blaze_items::{
    ItemId, ItemKey,
    key::{self, Resolved},
};
use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId,
    expr_store::{Body, ExpressionStore},
    hir::{ClosureKind, Expr, ExprId},
};
use hir_ty::{
    ImplTraitId, InferBodyId,
    body_store::{closure_kind_code, closure_kind_of},
    db::{
        InternedClosure, InternedClosureId, InternedCoroutineClosureId, InternedCoroutineId,
        InternedOpaqueTyId,
    },
    next_solver::{ImplTraitIdx, SolverDefId},
};
use la_arena::{Idx, RawIdx};

/// What a relative key begins with.
const RELATIVE: [u8; 20] = *b"blaze v2 relative: \0";

/// What a relative key names, relative to the body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Rel {
    /// The body's own item (a discriminant's variant).
    Own = 0,
    /// A closure written in the body, by its expression.
    Closure = 1,
    Coroutine = 2,
    CoroutineClosure = 3,
    /// An `impl Trait` of the body's own function's return type, by its index.
    Opaque = 4,
}

impl Rel {
    fn of(tag: u8) -> Option<Rel> {
        Some(match tag {
            0 => Rel::Own,
            1 => Rel::Closure,
            2 => Rel::Coroutine,
            3 => Rel::CoroutineClosure,
            4 => Rel::Opaque,
            _ => return None,
        })
    }
}

/// The key of `def` in a record of `body`: relative to `body` where it can be (see the module
/// doc), else its item key, the same in every database.
pub fn def_key<'db>(db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
    match relative(db, body, def) {
        Some((rel, at, kind)) => {
            let mut bytes = [0; 32];
            bytes[..20].copy_from_slice(&RELATIVE);
            bytes[20] = rel as u8;
            bytes[21..25].copy_from_slice(&at.to_le_bytes());
            bytes[25..28].copy_from_slice(&kind);
            ContentId(bytes)
        }
        None => key::def_key(db, def).0,
    }
}

/// What `def` is relative to `body`, where it is the body's own: which, its index, and a
/// closure's kind (zeros for anything else).
fn relative<'db>(
    db: &'db Db,
    body: DefWithBodyId,
    def: SolverDefId<'db>,
) -> Option<(Rel, u32, [u8; 3])> {
    let own = InferBodyId::DefWithBodyId(body);
    let closure = |rel: Rel, loc: InternedClosure<'db>| {
        (loc.owner == own).then(|| {
            let expr: ExprId = loc.expr;
            (rel, expr.into_raw().into_u32(), closure_kind_code(loc.kind))
        })
    };
    let none = [0; 3];
    match def {
        SolverDefId::FunctionId(it) if body == DefWithBodyId::FunctionId(it) => {
            Some((Rel::Own, 0, none))
        }
        SolverDefId::ConstId(it) if body == DefWithBodyId::ConstId(it) => Some((Rel::Own, 0, none)),
        SolverDefId::StaticId(it) if body == DefWithBodyId::StaticId(it) => {
            Some((Rel::Own, 0, none))
        }
        SolverDefId::EnumVariantId(it) if body == DefWithBodyId::VariantId(it) => {
            Some((Rel::Own, 0, none))
        }
        SolverDefId::InternedClosureId(it) => closure(Rel::Closure, it.loc(db)),
        SolverDefId::InternedCoroutineId(it) => closure(Rel::Coroutine, it.loc(db)),
        SolverDefId::InternedCoroutineClosureId(it) => closure(Rel::CoroutineClosure, it.loc(db)),
        SolverDefId::InternedOpaqueTyId(it) => match it.loc(db) {
            ImplTraitId::ReturnTypeImplTrait(function, index)
                if body == DefWithBodyId::FunctionId(function) =>
            {
                Some((Rel::Opaque, index.into_raw().into_u32(), none))
            }
            ImplTraitId::ReturnTypeImplTrait(..) | ImplTraitId::TypeAliasImplTrait(..) => None,
        },
        _ => None,
    }
}

/// The definition of `db` whose [`def_key`] in a record of `body` is `key`, `None` when it has
/// none (or when it is an anonymous constant's, see the module doc).
pub fn def_of<'db>(db: &'db Db, body: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>> {
    if key.0[..20] == RELATIVE {
        let def = relative_def(db, body, key)?;
        debug_assert_eq!(def_key(db, body, def), key, "a relative key names what has another");
        return Some(def);
    }
    let def = match key::resolve(db, ItemKey(key))? {
        Resolved::Item(item) => item_def(item)?,
        Resolved::Module(_) => return None,
        Resolved::Variant(variant) => SolverDefId::EnumVariantId(variant),
        Resolved::BuiltinDerive(it) => SolverDefId::BuiltinDeriveImplId(it),
        Resolved::Ctor(it) => SolverDefId::Ctor(it),
        Resolved::AnonConst { .. } => return None,
        Resolved::Opaque { owner, index } => {
            let index = ImplTraitIdx::from_raw(RawIdx::from_u32(index));
            let loc = match owner {
                ItemId::Function(it) => ImplTraitId::ReturnTypeImplTrait(it, index),
                ItemId::Alias(it) => ImplTraitId::TypeAliasImplTrait(it, index),
                _ => return None,
            };
            SolverDefId::InternedOpaqueTyId(InternedOpaqueTyId::new(db, loc))
        }
        Resolved::Closure { store, expr, kind } => {
            let loc = InternedClosure { owner: closure_owner(db, store, expr)?, expr, kind };
            match kind {
                ClosureKind::Closure => {
                    SolverDefId::InternedClosureId(InternedClosureId::new(db, loc))
                }
                ClosureKind::OldCoroutine(_) | ClosureKind::Coroutine { .. } => {
                    SolverDefId::InternedCoroutineId(InternedCoroutineId::new(db, loc))
                }
                // Two keys resolve here: the coroutine closure's own and the closure's whose
                // type it has when called.
                ClosureKind::CoroutineClosure(_) => {
                    let it = InternedCoroutineClosureId::new(db, loc);
                    match key::coroutine_closure_key(db, it).0 == key {
                        true => SolverDefId::InternedCoroutineClosureId(it),
                        false => SolverDefId::InternedClosureId(it.as_closure(db)),
                    }
                }
            }
        }
    };
    debug_assert_eq!(key::def_key(db, def).0, key, "a key resolves to what has another key");
    Some(def)
}

/// [`def_of`] of a relative key.
fn relative_def<'db>(db: &'db Db, body: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>> {
    let at = u32::from_le_bytes(key.0[21..25].try_into().expect("four bytes"));
    let own = InferBodyId::DefWithBodyId(body);
    let kind: [u8; 3] = key.0[25..28].try_into().expect("three bytes");
    if key.0[28..] != [0; 4] {
        return None;
    }
    // The closure is the one the record's writer saw at this expression of this body: the
    // record was found under a key that fixes the body's source and everything its lowering
    // read, and lowering is a function of those, so the expression is that closure here too.
    // No store is opened to see it (a store is a lowering).
    let closure = |at: u32| {
        let expr: ExprId = Idx::from_raw(RawIdx::from_u32(at));
        Some(InternedClosure { owner: own, expr, kind: closure_kind_of(kind).ok()? })
    };
    let rel = Rel::of(key.0[20])?;
    if matches!(rel, Rel::Own | Rel::Opaque) && kind != [0; 3] {
        return None;
    }
    Some(match rel {
        Rel::Own => match body {
            DefWithBodyId::FunctionId(it) => SolverDefId::FunctionId(it),
            DefWithBodyId::ConstId(it) => SolverDefId::ConstId(it),
            DefWithBodyId::StaticId(it) => SolverDefId::StaticId(it),
            DefWithBodyId::VariantId(it) => SolverDefId::EnumVariantId(it),
        },
        Rel::Closure => SolverDefId::InternedClosureId(InternedClosureId::new(db, closure(at)?)),
        Rel::Coroutine => {
            SolverDefId::InternedCoroutineId(InternedCoroutineId::new(db, closure(at)?))
        }
        Rel::CoroutineClosure => SolverDefId::InternedCoroutineClosureId(
            InternedCoroutineClosureId::new(db, closure(at)?),
        ),
        Rel::Opaque => {
            let DefWithBodyId::FunctionId(function) = body else { return None };
            let index = ImplTraitIdx::from_raw(RawIdx::from_u32(at));
            let loc = ImplTraitId::ReturnTypeImplTrait(function, index);
            SolverDefId::InternedOpaqueTyId(InternedOpaqueTyId::new(db, loc))
        }
    })
}

fn item_def<'db>(item: ItemId) -> Option<SolverDefId<'db>> {
    Some(match item {
        ItemId::Function(it) => SolverDefId::FunctionId(it),
        ItemId::Adt(it) => SolverDefId::AdtId(it),
        ItemId::Trait(it) => SolverDefId::TraitId(it),
        ItemId::Impl(it) => SolverDefId::ImplId(it),
        ItemId::Const(it) => SolverDefId::ConstId(it),
        ItemId::Static(it) => SolverDefId::StaticId(it),
        ItemId::Alias(it) => SolverDefId::TypeAliasId(it),
        ItemId::Macro(_) => return None,
    })
}

/// What inferred the closure at `expr` of `store`: the body, for one in the body's own
/// expression or one of its `const { .. }` blocks (the body infers those); `None` for one in
/// another root, an anonymous constant that is inferred alone and is its owner.
fn closure_owner<'db>(
    db: &'db Db,
    store: ExpressionStoreOwnerId,
    expr: ExprId,
) -> Option<InferBodyId<'db>> {
    // A signature's or a variant's fields' closures are all in its anonymous constants.
    let ExpressionStoreOwnerId::Body(def) = store else { return None };
    let exprs = ExpressionStore::of(db, store);
    let root = exprs.find_root_for_expr(expr);
    let inferred_by_body = root == Body::of(db, def).root_expr()
        || exprs.exprs().any(|(_, it)| matches!(it, Expr::Const(inline) if inline == root));
    inferred_by_body.then_some(InferBodyId::DefWithBodyId(def))
}
