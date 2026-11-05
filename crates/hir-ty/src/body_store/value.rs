//! A body's step as a fact's value (docs/v2/plan/design/item-hash.md section 5): the hash of the
//! step's result in the item store's encoding, which names items by [`BodyFacts::def_key`], so
//! two databases' values are equal exactly when their results are. A later step of the body
//! reads the earlier one's queries; lane item-hash's classification files those reads as this
//! fact.

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId,
    expr_store::ExpressionStore,
    hir::{ClosureKind, CoroutineKind, Expr},
};
use rustc_type_ir::inherent::IntoKind;

use super::{BodyFacts, Step, infer, mir, with_store};
use crate::{
    InferBodyId, InferenceResult,
    mir::{MirOwner, MirStage},
    next_solver::TyKind,
    providers::mir_at,
};

const INFER_DOMAIN: &str = "blaze v2 item store step value 1 inference";
const MIR_DOMAIN: &str = "blaze v2 item store step value 1 MIR";

/// The value of `step` of `def`: the hash of its result as the item store encodes it, `None`
/// when it has no stored form, when no item store is installed for `db` (the encoding names
/// items through the store's [`BodyFacts`]) and for [`Step::Borrowck`], whose verdict no later
/// step reads.
///
/// [`Step::Mir`] covers, in this order, the body's built MIR and that of every closure and
/// coroutine written in it, reached or not, in the order of their expressions; each by its kind
/// and expression. A `const { .. }` block's MIR is not in it.
pub fn step_value(db: &Db, def: DefWithBodyId, step: Step) -> Option<ContentId> {
    match step {
        Step::Infer => inference_value(db, def),
        Step::Mir => mir_value(db, def),
        Step::Borrowck => None,
    }
}

/// [`step_value`] of [`Step::Infer`]. A store is installed for a database before any query of it
/// runs and stays for its life, so which store answers is no input.
#[blaze_query::tracked(returns(copy))]
fn inference_value(db: &Db, def: DefWithBodyId) -> Option<ContentId> {
    with_store(db, |store| {
        let result = InferenceResult::of(db, def);
        // alloc: one answer's bytes, once per body a revision, memoized
        let mut bytes = Vec::new();
        infer::encode(db, &*store.facts, def, result, &mut bytes).ok()?;
        Some(Hasher::new(INFER_DOMAIN).part(&bytes).finish())
    })
    .flatten()
}

/// [`step_value`] of [`Step::Mir`], as [`inference_value`].
#[blaze_query::tracked(returns(copy))]
fn mir_value(db: &Db, def: DefWithBodyId) -> Option<ContentId> {
    with_store(db, |store| mir_hash(db, &*store.facts, def)).flatten()
}

fn mir_hash(
    db: &Db,
    // dyn: the store's facts, as `mir::encode` takes them
    facts: &dyn BodyFacts,
    def: DefWithBodyId,
) -> Option<ContentId> {
    let mut hasher = Hasher::new(MIR_DOMAIN);
    // alloc: one answer's bytes at a time, reused; once per body a revision, memoized
    let mut bytes = Vec::new();
    let mut add = |tag: u8, expr: u32, owner: MirOwner<'_>| -> Option<()> {
        let body = mir_at(db, owner, MirStage::Built).ok()?;
        bytes.clear();
        mir::encode(db, facts, def, body, &mut bytes).ok()?;
        hasher.update(&[tag]).update(&expr.to_le_bytes()).part(&bytes);
        Some(())
    };
    add(0, u32::MAX, MirOwner::Body(InferBodyId::DefWithBodyId(def)))?;
    let infer = InferenceResult::of(db, def);
    let store = ExpressionStore::of(db, ExpressionStoreOwnerId::Body(def));
    for (expr, written) in store.exprs() {
        // The kinds MIR lowering makes a body of, as the borrow check finds them.
        let Expr::Closure {
            closure_kind:
                ClosureKind::Closure
                | ClosureKind::Coroutine { kind: CoroutineKind::Async, .. }
                | ClosureKind::CoroutineClosure(CoroutineKind::Async),
            ..
        } = written
        else {
            continue;
        };
        let at = expr.into_raw().into_u32();
        match infer.expr_ty(expr).kind() {
            TyKind::Closure(id, _) => add(1, at, MirOwner::Closure(id.0))?,
            TyKind::CoroutineClosure(id, _) => add(1, at, MirOwner::Closure(id.0.as_closure(db)))?,
            TyKind::Coroutine(id, _) => add(2, at, MirOwner::Coroutine(id.0))?,
            _ => {}
        }
    }
    Some(hasher.finish())
}
