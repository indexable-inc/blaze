//! The identity of a pure script (design section 4): every instance its walk reached, in walk
//! order, each by what its code is and the places of its callees in that order. Moving the
//! script or reformatting it changes no part; a changed body anywhere it reaches, a call that
//! resolves to another function, or a changed fact a body's code reads (a type's layout or
//! drop impl, a constant's or static's value) changes one.

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::{ContentId, Hasher};
use blaze_item_hash::identity::identity;
use blaze_items::{ItemId, item_key};
use blaze_query::db::FromId;
use hir_def::{
    DefWithBodyId, StaticId,
    signatures::{StaticFlags, StaticSignature},
};
use hir_ty::{
    InferBodyId, MemoryMap, ParamEnvAndCrate,
    db::{ClosureKey, HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
    next_solver::{
        AllocationData, ConstKind, DbInterner, Ty, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{MirBody, Operand, OperandKind};
use hir_ty_share::identity as share;

use crate::{
    models::tag,
    values::evaluate,
    walk::{Def, Instance, body_of},
};

pub(crate) const DOMAIN: &str = "blaze v2 pure script identity 1";
/// The domain of one instance's part of a script's identity.
pub(crate) const INSTANCE_DOMAIN: &str = "blaze v2 pure script instance 1";

/// The identity of the instances `order` (walk order, `main` first) in `env`, whose callees,
/// in call-site order, are `callees`; `index` is each instance's place in `order`. An error
/// says which body has none.
pub(crate) fn of(
    db: &Db,
    env: &StoredParamEnvAndCrate,
    order: &[Instance],
    callees: &[&[Instance]],
    index: &HashMap<Instance, u32>,
) -> Result<ContentId, String> {
    let mut h = Hasher::new(DOMAIN);
    h.update(&(order.len() as u32).to_le_bytes());
    for (instance, callees) in order.iter().zip(callees) {
        // clone: the query's key, reference counts
        match content(db, env.clone(), instance.clone()) {
            Ok(id) => h.id(id),
            Err(what) => return Err(what.clone()), // clone: an impure script's reason, once
        };
        h.update(&(callees.len() as u32).to_le_bytes());
        for callee in *callees {
            h.update(&index[callee].to_le_bytes());
        }
    }
    Ok(h.finish())
}

/// What the code of `instance` in `env` is: its body's identity, its arguments' identity, and
/// the facts its monomorphized body reads past its body's identity: each local's type, and
/// each constant's and static's value.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn content(
    db: &Db,
    env: StoredParamEnvAndCrate,
    instance: Instance,
) -> Result<ContentId, String> {
    let mut h = Hasher::new(INSTANCE_DOMAIN);
    match instance.def {
        Def::Fn(func) => {
            h.update(&[1]).id(&body(db, func.into())?);
        }
        Def::Closure(id) => {
            let loc = InternedClosureId::from_id(id).loc(db);
            h.update(&[2]);
            closure(db, &mut h, loc)?;
        }
        Def::Coroutine(id) | Def::CoroutineByMove(id) => {
            let loc = InternedCoroutineId::from_id(id).loc(db);
            h.update(&[if matches!(instance.def, Def::Coroutine(_)) { 3 } else { 4 }]);
            closure(db, &mut h, loc)?;
        }
        Def::Leaf(func, leaf) => {
            h.update(&[5]).id(&item_key(db, ItemId::Function(func)).0).update(&[tag(leaf)]);
            return Ok(h.finish());
        }
    }
    let args = share::args(db, instance.args.as_ref())
        .map_err(|it| format!("its type arguments have no identity: {it}"))?; // alloc: an impure script's reason, once
    h.id(&args);
    let body = match body_of(db, &env, &instance) {
        Some(Ok(body)) => body,
        // alloc: an impure script's reason, once
        Some(Err(error)) => return Err(format!("has a body that does not lower to MIR: {error:?}")),
        None => return Ok(h.finish()),
    };
    let env = env.as_ref(db);
    let infcx = DbInterner::new_with(db, env.krate).infer_ctxt().build(TypingMode::Codegen);
    let mut facts = Facts {
        db,
        infcx,
        env,
        h,
        tys: HashSet::default(),
        statics: HashSet::default(),
    };
    facts.body(body)?;
    Ok(facts.h.finish())
}

/// A closure's or coroutine's part: its parent body's identity and its place among the
/// closures of that body.
fn closure(db: &Db, h: &mut Hasher, loc: InternedClosure<'_>) -> Result<(), String> {
    let InferBodyId::DefWithBodyId(parent) = loc.owner else {
        // alloc: an impure script's reason, once
        return Err("has a closure in a constant's expression, whose body has no identity".to_owned());
    };
    h.id(&body(db, parent)?).update(&ClosureKey::nth(db, loc).to_le_bytes());
    Ok(())
}

fn body(db: &Db, def: DefWithBodyId) -> Result<ContentId, String> {
    // alloc: an impure script's reason, once
    identity(db, def).map_err(|it| format!("its body has no identity: {}", it.what))
}

/// The facts one monomorphized body reads, streamed into its instance's hash in body order,
/// each type and static once.
struct Facts<'db> {
    db: &'db Db,
    infcx: InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    h: Hasher,
    tys: HashSet<Ty<'db>>,
    statics: HashSet<StaticId>,
}

impl<'db> Facts<'db> {
    fn body(&mut self, body: &MirBody<'db>) -> Result<(), String> {
        for (_, local) in body.locals.iter() {
            self.ty(local.ty.as_ref())?;
        }
        let mut failed = None;
        let mut visit = |operand: &Operand| {
            if failed.is_none()
                && let Err(what) = self.operand(operand)
            {
                failed = Some(what);
            }
        };
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                statement.kind.for_each_operand(&mut visit);
            }
            if let Some(terminator) = &block.terminator {
                terminator.kind.for_each_operand(&mut visit);
            }
        }
        failed.map_or(Ok(()), Err)
    }

    fn operand(&mut self, operand: &Operand) -> Result<(), String> {
        match &operand.kind {
            OperandKind::Copy(_) | OperandKind::Move(_) => Ok(()),
            OperandKind::Static(id) => self.static_value(*id),
            OperandKind::Constant { konst, ty } => {
                let (konst, ty) = (konst.as_ref(), ty.as_ref());
                self.ty(ty)?;
                if matches!(konst.kind(), ConstKind::Alias(..)) {
                    let value = evaluate(self.db, &self.infcx, self.env, konst)?;
                    self.h.update(&[2]);
                    self.allocation(&value)
                } else {
                    let id = share::konst(self.db, konst)
                        .map_err(|it| format!("the constant `{konst:?}` has no identity: {it}"))?; // alloc: an impure script's reason, once
                    self.h.update(&[3]).id(&id);
                    Ok(())
                }
            }
            OperandKind::Allocation { allocation } => {
                let value = allocation.as_ref();
                self.ty(value.ty)?;
                self.h.update(&[4]);
                self.allocation(&value)
            }
        }
    }

    fn ty(&mut self, ty: Ty<'db>) -> Result<(), String> {
        if self.tys.insert(ty) {
            let id = self.ty_id(ty)?;
            self.h.update(&[1]).id(&id);
        }
        Ok(())
    }

    fn ty_id(&self, ty: Ty<'db>) -> Result<ContentId, String> {
        share::ty(self.db, ty).map_err(|it| format!("the type `{ty:?}` has no identity: {it}")) // alloc: an impure script's reason, once
    }

    /// A static by its key and, unless it is foreign, its value.
    fn static_value(&mut self, id: StaticId) -> Result<(), String> {
        if !self.statics.insert(id) {
            return Ok(());
        }
        self.h.update(&[5]).id(&item_key(self.db, ItemId::Static(id)).0);
        if StaticSignature::of(self.db, id).flags.contains(StaticFlags::EXTERN) {
            return Ok(());
        }
        let value = self.db.const_eval_static(id).map_err(|error| {
            format!("reads `{}`, whose value does not evaluate: {error:?}", StaticSignature::of(self.db, id).name.as_str()) // alloc: an impure script's reason, once
        })?;
        self.allocation(&value)
    }

    /// An evaluated value: its bytes and every piece of memory, vtable, static and type id it
    /// holds.
    fn allocation(&mut self, value: &AllocationData<'db>) -> Result<(), String> {
        self.h.part(&value.memory);
        match &value.memory_map {
            MemoryMap::Empty => {
                self.h.update(&[0]);
            }
            MemoryMap::Simple(bytes) => {
                self.h.update(&[1]).part(bytes);
            }
            MemoryMap::Complex(map) => {
                self.h.update(&[2]).update(&(map.memory.iter().len() as u64).to_le_bytes());
                for (address, bytes) in map.memory.iter() {
                    self.h.update(&(*address as u64).to_le_bytes()).part(bytes);
                }
                self.h.update(&(map.vtable.tys().len() as u64).to_le_bytes());
                for ty in map.vtable.tys() {
                    let id = self.ty_id(ty)?;
                    self.h.id(&id);
                }
                self.h.update(&(map.type_ids.len() as u64).to_le_bytes());
                for ty in &map.type_ids {
                    match ty {
                        Some(ty) => {
                            let id = self.ty_id(*ty)?;
                            self.h.update(&[1]).id(&id);
                        }
                        None => {
                            self.h.update(&[0]);
                        }
                    }
                }
                self.h.update(&(map.statics.len() as u64).to_le_bytes());
                for memory in &map.statics {
                    self.h
                        .update(&(memory.start as u64).to_le_bytes())
                        .update(&(memory.size as u64).to_le_bytes())
                        .id(&item_key(self.db, ItemId::Static(memory.id)).0);
                }
                for memory in &map.statics {
                    self.static_value(memory.id)?;
                }
            }
        }
        Ok(())
    }
}
