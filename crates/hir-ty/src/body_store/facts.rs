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
    /// `const_eval_query` of a `const` item with no generic arguments: its value
    /// (`super::konst`).
    Const = 3,
    /// [`super::lowering`]: the small facts of the body's lowering (its node counts, its
    /// closures with their kinds, whether it has lowering errors or location literals), what
    /// the other steps' decoders and a closure's interning read of the body, so a hit of
    /// another step lowers nothing. Its value as a fact is the digest of what the lowering
    /// read ([`super::Lowered::value`]), not of its answer: two bodies with equal counts can
    /// have read different things.
    Lower = 4,
}

impl Step {
    pub const ALL: [Step; 5] = [Step::Infer, Step::Mir, Step::Borrowck, Step::Const, Step::Lower];
    /// How many steps there are: the length of an array indexed by a step.
    pub const COUNT: usize = Step::ALL.len();

    /// Whether the step's answers come from the kernel's verified body entries
    /// ([`super::entries`], posted by [`super::entries::serve`]), not from the item store's
    /// own records: every step but a constant's value and the lowering, which have no entry
    /// yet (`blaze_kernel::steps::BODY_CONST` is the first's number, owed).
    pub const fn served(self) -> bool {
        !matches!(self, Step::Const | Step::Lower)
    }

    /// Whether `self` is a step that may read `earlier` of the same body as a fact: the
    /// lowering is earlier than every other step; otherwise by number.
    pub const fn reads(self, earlier: Step) -> bool {
        match (self, earlier) {
            (Step::Lower, _) => false,
            (_, Step::Lower) => true,
            _ => (earlier as u8) < (self as u8),
        }
    }
}

/// What one read a step made stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Classified {
    /// Its value is a function of the body's identity: nothing to record.
    Covered,
    /// It stands for this fact, whose value goes into the record's key.
    Fact(FactRef),
    /// As [`Classified::Fact`], of a read that took a whole table where a lookup would do (an
    /// iteration of a module's scope): its fact changes with any row of the table, so the
    /// flush counts it by who read it ([`Flushed::scans`](super::Flushed::scans)).
    Scan(FactRef),
    /// Its value is a function of what its own row read and nothing else (a type's layout,
    /// and the derived tables a layout reads): the store classifies the row's kept reads in
    /// its place, through any of them that is `Through` again, down to reads that are
    /// covered, facts or refused. So what decides the value is traced as the facts there
    /// already are, each keyed by an item or a crate, and nothing is keyed by the read's own
    /// arguments (a type, whose interned id is one process's). Any function's row may be
    /// given this: the walk brings it up to date by its key (`blaze_query::db::refresh`).
    /// One a store answered, or that cannot be brought up to date, is refused by name.
    Through,
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

    /// The value of `fact` now as `def`'s crate sees it (a fact about impls covers only the
    /// crates it loads); `None` when it cannot be hashed now.
    fn value(&self, db: &Db, def: DefWithBodyId, fact: FactRef) -> Option<ContentId>;

    /// What a read by a step of `def` of a row behind the seal of the library group `group`
    /// stands for, the row having let go of its reads there (`blaze_query::db::RowRead::Sealed`,
    /// a row the walk would have read through): the facts its value is a function of, pushed
    /// onto `facts`; `false` when none stand for it, and the step is refused by the row's name.
    /// The default stands for none.
    fn sealed(
        &self,
        db: &Db,
        def: DefWithBodyId,
        group: blaze_query::db::SealGroup,
        facts: &mut Vec<FactRef>,
    ) -> bool {
        let _ = (db, def, group, facts);
        false
    }

    /// The earlier step of the body itself that `fact` stands for, where it is one (a body's
    /// MIR reads its own inference): a kernel entry reads that step's entry for it
    /// ([`super::entries`]), not a fact of the world. `None` for every other fact.
    fn own_step(&self, fact: FactRef) -> Option<Step> {
        let _ = fact;
        None
    }

    /// The key of a definition `body`'s record names: `body` itself and what it owns (closures
    /// with their kind, coroutines, opaque types) as "self", relative to `body`; anything else,
    /// an item one of its paths names among them, by its plain item key, the same in every
    /// database. Reading a key back ([`BodyFacts::def_of`]) lowers no body.
    fn def_key<'db>(&self, db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId;

    /// The definition [`BodyFacts::def_key`] gave `key` for `body` in `db`; `None` when there is
    /// none: the record names an item this database does not have, which for a record found
    /// under a matching identity is a bug in the identity.
    fn def_of<'db>(
        &self,
        db: &'db Db,
        body: DefWithBodyId,
        key: ContentId,
    ) -> Option<SolverDefId<'db>>;
}
