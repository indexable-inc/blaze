//! What the item store asks above hir-ty (docs/v2/plan/design/item-hash.md sections 3 and 6):
//! a body's identity, what each read of a step stands for, a fact's value now, and the item keys
//! a record's ids are written as. Lane item-hash's `blaze_item_hash::Bodies` gives it; hir-ty
//! cannot name `blaze_items`.

use base_db::Db;
use blaze_ids::ContentId;
use hir_def::DefWithBodyId;

use crate::next_solver::{SolverDefId, goal_store::FactRef};

/// Which step of a body a record is the result of.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Step {
    /// `infer_body_query`: the body's `InferenceResult`.
    Infer = 0,
    /// `mir_body_query`: the body's MIR as built, before optimization.
    Mir = 1,
    /// `hir_ty_borrowck::gate::checked`: that the borrow check passed.
    Borrowck = 2,
}

impl Step {
    pub const ALL: [Step; 3] = [Step::Infer, Step::Mir, Step::Borrowck];
}

/// What one read a step made stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Classified {
    /// Its value is a function of the body's identity: nothing to record.
    Covered,
    /// It stands for this fact, whose value goes into the record's key.
    Fact(FactRef),
    /// It stands for nothing the store can key: the result is not stored.
    Refused,
}

/// Lane item-hash's side of the item store. Every method may be called from any worker at once;
/// none is called on a hit but [`BodyFacts::identity`] and [`BodyFacts::value`], both memoized
/// queries.
pub trait BodyFacts: Send + Sync + 'static {
    /// The identity of `def`'s body (section 3): `None` when it has none (a part of it could not
    /// be hashed: an error type, a block-local item, a body that failed to lower). A body with no
    /// identity is never looked up nor stored.
    fn identity(&self, db: &Db, def: DefWithBodyId) -> Option<ContentId>;

    /// What the read `read` (of kind `kind`, its ingredient named `name`) made by `step` of
    /// `def` stands for (section 6). Interned reads are never asked.
    fn classify(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        read: blaze_query::db::DatabaseKeyIndex,
        kind: blaze_query::db::ReadKind,
        name: &'static str,
    ) -> Classified;

    /// The value of `fact` now; `None` when it cannot be hashed now.
    fn value(&self, db: &Db, fact: FactRef) -> Option<ContentId>;

    /// The key of a definition `body`'s record names, relative to `body`: `body` itself and what
    /// it owns (closures, coroutines, opaque types, anonymous constants) as "self", an item one of
    /// its paths names as "the target of that path", anything else by its item key. Two bodies of
    /// one identity give the same item the same key in every database, under any renaming.
    fn def_key<'db>(&self, db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId;

    /// The definition [`BodyFacts::def_key`] gave `key` for `body` in `db`; `None` when there is
    /// none: the record names an item this database does not have, which for a record found
    /// under a matching identity is a bug in the identity.
    fn def_of<'db>(&self, db: &'db Db, body: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>>;
}
