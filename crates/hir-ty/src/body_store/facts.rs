//! What the item store asks above hir-ty (docs/v2/plan/design/item-hash.md sections 3 and 6):
//! a body's identity, what each entry of a step's read list stands for, a fact's value now, and the item keys
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

/// What one row of a step's read list stands for. The list is the recorder's
/// (`blaze_memo::record`, front-end-on-memo rule 5): each input the run read and each call of
/// a fact boundary kind, every other query flattened into it by the memo, so there is no row
/// to read through and no seal (rule 1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Classified {
    /// Its value is a function of the body's identity: nothing to record.
    Covered,
    /// It stands for this fact, whose value goes into the record's key.
    Fact(FactRef),
    /// As [`Classified::Fact`], of a read that took a whole table where a lookup would do (an
    /// iteration of a module's scope), with the name of what was read: its fact changes with
    /// any row of the table, so the flush counts it by who read it
    /// ([`Flushed::scans`](super::Flushed::scans)).
    Scan(FactRef, &'static str),
    /// It stands for nothing the store can key, and is a read of what is named: the result
    /// is not stored, counted by that name ([`Flushed::refused`](super::Flushed::refused)).
    Refused(&'static str),
}

/// Lane item-hash's side of the item store. Every method may be called from any worker at once;
/// none is called on a hit but [`BodyFacts::identity`] and [`BodyFacts::value`], both memoized
/// queries.
pub trait BodyFacts: Send + Sync + 'static {
    /// The identity of `def`'s body (section 3): `None` when it has none (a part of it could not
    /// be hashed: an error type, a block-local item, a body that failed to lower). A body with no
    /// identity is never looked up nor stored.
    fn identity(&self, db: &Db, def: DefWithBodyId) -> Option<ContentId>;

    /// What `read`, one row of the read list of `step` of `def` (`blaze_memo::record`),
    /// stands for (section 6; port decision 2): a `Read::Query` row is a call of a fact
    /// boundary kind (its own reads are not in the list), mapped by its kind and key to the
    /// stable fact of lane items whose value changes whenever its answer does; a
    /// `Read::Input` row by its input's kind (the body's own file text covered by its
    /// syntax, a process input refused), and the stored answer a hit took
    /// ([`super::STORED_ANSWER`]) by that kind. Asker-specific answers (`Covered` for the
    /// asker's own items) are decided here, at offer time. What is filed is the answer's
    /// [`FactRef`]s and their values alone, never a `Gid` or a kind id.
    fn fact_of(&self, db: &Db, def: DefWithBodyId, step: Step, read: blaze_memo::Read)
    -> Classified;

    /// The value of `fact` now as `def`'s crate sees it (a fact about impls covers only the
    /// crates it loads); `None` when it cannot be hashed now.
    fn value(&self, db: &Db, def: DefWithBodyId, fact: FactRef) -> Option<ContentId>;

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
