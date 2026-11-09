//! What code identity hashes of one constant's or static's value, edge by edge: each vtable as
//! its concrete type and `dyn`, and each function or closure a pointer in it runs. Test support
//! for hir-ty-codegen's check that every vtable and function its relocation of the value emits
//! is among these (rule 21), so the two walks of a value can't drift apart unseen. Not an
//! interface: nothing but that test reads it.

use base_db::Db;
use blaze_ids::Hasher;
use hir_def::{ConstId, FunctionId, HasModule, StaticId};
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    next_solver::{GenericArgs, ParamEnv, StoredGenericArgs, StoredTy},
};
use hir_ty_mir::mir::dispatch::Body;

use crate::{
    facts::{Cx, Edge, Facts, Holder},
    instance::erased,
};

/// The value whose edges are asked for.
#[derive(Clone, Copy, Debug)]
pub enum ValueOf {
    Static(StaticId),
    /// A constant with no generic parameters.
    Const(ConstId),
}

/// One edge of a value's facts, its types erased as the facts hash them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Hashed {
    /// A vtable of `concrete` for `dyn_ty`, whose slots' targets are hashed.
    Vtable { concrete: StoredTy, dyn_ty: StoredTy },
    /// A function item a pointer in the value runs, with its arguments.
    Call { func: FunctionId, args: StoredGenericArgs },
    /// A closure a pointer in the value runs, with its parent's arguments.
    Closure { body: Body, parent: StoredGenericArgs },
    /// Drop glue of the type.
    Drop(StoredTy),
}

/// The edges code identity hashes of `of`'s value, in its walk's order, as a body that reads
/// the static or the constant hashes them; or why the value has no identity. Run attached to
/// `db` (`blaze_query::db::attach`).
pub fn value_edges(db: &Db, of: ValueOf) -> Result<Vec<Hashed>, String> {
    let krate = match of {
        ValueOf::Static(id) => id.krate(db),
        ValueOf::Const(id) => id.krate(db),
    };
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
    let cx = Cx::new(db, &env);
    let mut facts = Facts::new(&cx, Hasher::new("blaze v2 code identity value edges (test)"));
    match of {
        ValueOf::Static(id) => facts.static_use(id),
        ValueOf::Const(id) => {
            let value = db
                .const_eval(id, GenericArgs::empty(), Some(cx.env))
                .map_err(|error| format!("the constant does not evaluate: {error:?}"))?; // alloc: error path
            facts.ty(value.ty);
            facts.allocation(&value, value.ty, Holder::Body);
        }
    }
    if let Some(why) = facts.failed {
        return Err(why);
    }
    Ok(facts
        .edges
        .iter()
        .map(|edge| match *edge {
            Edge::Vtable(concrete, dyn_ty) => Hashed::Vtable {
                concrete: erased(cx.interner, concrete).store(),
                dyn_ty: erased(cx.interner, dyn_ty).store(),
            },
            Edge::Call(func, args) => {
                Hashed::Call { func, args: erased(cx.interner, args).store() }
            }
            Edge::Closure(body, parent) => {
                Hashed::Closure { body, parent: erased(cx.interner, parent).store() }
            }
            Edge::Drop(ty) => Hashed::Drop(erased(cx.interner, ty).store()),
        })
        .collect()) // alloc: test support, the edges handed out
}
