//! A body's step as a fact's value (docs/v2/plan/design/item-hash.md section 5): the hash of the
//! step's result in the item store's encoding, which names items by [`BodyFacts::def_key`], so
//! two databases' values are equal exactly when their results are. A later step of the body
//! reads the earlier one's queries; lane item-hash's classification files those reads as this
//! fact.

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use hir_def::DefWithBodyId;

use super::{BodyFacts, Step, infer, mir, with_store};
use crate::{InferenceResult, mir::MirStage, providers::mir_at};

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
        // A reader of a constant reads its `ConstValue` fact, not its step.
        Step::Borrowck | Step::Const => None,
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
    for (place, owner) in mir::owners(db, def) {
        let body = mir_at(db, owner, MirStage::Built).ok()?;
        bytes.clear();
        mir::encode(db, facts, def, body, &mut bytes).ok()?;
        // Its tag and its expression as a `u32`: an expression's index is one, and the body's
        // `NONE` (all ones) is `u32::MAX`, the bytes this value always hashed.
        let expr = u32::try_from(place.expr).unwrap_or(u32::MAX);
        hasher.update(&[place.tag]).update(&expr.to_le_bytes()).part(&bytes);
    }
    Some(hasher.finish())
}
