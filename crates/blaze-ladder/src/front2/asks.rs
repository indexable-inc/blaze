//! D161 STEP 3 (docs/v2/plan/design/end-state.md, "a task that lacks a fact waits on it; nothing
//! starts over"): front2's restart loops moved onto blaze-ask's one ask, each behind ONE seam,
//! [`AsksMet`] (rule 78: the strategy is chosen at one place; tables/task.rs
//! [`compare_asks`](super::tables::task::compare_asks) runs a body both ways and asserts equal
//! answers), so deleting the old path is one edit per loop.
//!
//! - **Loop (1)**, world_items.rs `BodyWorld::again` over `store_want` and `impl_question`: under
//!   [`AsksMet::Once`] each head a seeded store imports on the spot is a [`Need`] asked ONCE on the
//!   store's [`Board`] ([`ask_seed`]): the first ask makes it (the seed is the maker's one step),
//!   any later ask of it in the store reads the published word. `again` asks the question again
//!   while [`Store::imports`] moves, bounded by the board's slots, not by
//!   `MOST_IMPORTS_A_QUESTION`. Under `Again`, `Store::imported` counts as before.
//! - **The gate's set** ([`Gate`]): computed from the body's syntactic prefetch (blaze-typeck
//!   world.rs `prefetch_syntactic`, which world_items.rs's `World::prefetch` hands here): every
//!   path the text names, resolved over the names record, the heads each resolution reads, the
//!   traits in scope of each method call and their members, the lang paths the desugarings name.
//!   Collected in round 0 before the walk enters the body, frozen as a [`GateSet`] after it, and
//!   read by tables/cost.rs `Cost::again` to class each fact met after entry
//!   ([`blaze_ask::After`]): `Named` (the set has it; settling it before entry takes it away) or
//!   `Gap` (the set's computation misses it).
//!
//! A [`Need`] here is the engine's cell in the checker's words (blaze-ask need.rs): a head's need
//! is fact `Head` (`Members` for a trait's members) of the want's bytes, which a body's
//! `Asked::Parked { on }` hands the engine as it is.

use core::cell::RefCell;
use core::fmt::Write as _;

use blaze_ask::{AskError, After, Asked, Asker, AskerOpts, Board, Counts, Detail, Fact, GateSet, Given, Maker, Need, Plant, Step, Subject};
use blaze_core::claim_table::{ClaimError, TaskId};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;

use super::heads::{Seeded, Store, Want};
use super::keys::Keys;
use super::tables::seeds::HeadSeed;

/// How a seeded store's world meets a head it lacks: THE seam of D161 step 3 (rule 78).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsksMet {
    /// The restart path: `Store::imported` counts each import and `BodyWorld::again` asks the
    /// question again while it moves, at most `MOST_IMPORTS_A_QUESTION` times.
    Again,
    /// The one ask: each import a need claimed once on the store's board.
    Once(AsksPlant),
}

/// The oracle's planted bug (tables/task.rs `compare_asks`'s control).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsksPlant {
    Clean,
    /// PLANTED BUG: a head the ask made is answered as refused, so the body meets a gap the
    /// restart path never meets. The oracle must see the two answers differ.
    RefuseMade,
}

/// The store's side of the seam: under `Once`, the board its asks claim on (rule 76: no `Once`
/// without a board).
///
/// WHERE THE SEAM CAN DIFFER (the oracle's coverage, D161 step 3): only over a SEEDED store.
/// Every read of the choice is behind `Store::seeds`: `BodyWorld::store_want` (`if let
/// Some(seeds)`), `BodyWorld::impl_question` (returns to the round's fill without seeds), and
/// check.rs `Importing::make` (an `Importing` is built only from seeds). The counts the rest
/// read (`Store::imports`, which `again` and the round loop watch) move only through those
/// three, so over a lowering store both are 0 under either choice, `again` returns on its
/// first pass and the round loop is the same. Seeds are set in one place, tables/task.rs
/// `record_body_as`, which `record_body_observed` (the oracle's entry) is the only way into:
/// every body whose check the choice can change is compared.
pub enum Asks {
    Again,
    Once {
        board: Board<Alloc<'static>>,
        plant: AsksPlant,
        /// The first ask refused in the store, kept for the question that met it: an impl
        /// question's import refusal is a gap of that question (rule 3: never dropped).
        refused: RefCell<Option<AskRefused>>,
    },
}

/// The one task id of a body's store: a store is one task's (its board is that task's alone).
pub const ONE_TASK: TaskId = TaskId(0);

/// The needs one body's board holds: past this, an ask is refused by name (`ClaimError::Full`,
/// rule 18). Every head a body imports is one; a body of tempo imports a few hundred.
pub const BOARD_SLOTS: u32 = 4096;

/// Maker steps per ask: a seed is one step, so an ask never yields.
const FUEL: u32 = 8;

/// The domain of a want's need subject.
const WANT_DOMAIN: &str = "front2 want need v1";

/// The domain of a checker question's need subject.
const QUESTION_DOMAIN: &str = "front2 question need v1";

/// Why an ask was refused (rule 19: the need, and what refused it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AskRefused {
    pub need: Need,
    pub why: &'static str,
}

impl core::fmt::Display for AskRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the one ask (blaze-ask, D161) refused {}: {}", self.need, self.why)
    }
}

/// What an import through the one ask came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Imported {
    /// Made now: the question is asked again at once.
    Made,
    /// Published before in this store (made or failed): nothing to make again.
    Read,
    /// The seed filed nothing, or refused: the round's fill meets it and keeps its refusal.
    Failed,
}

impl Asks {
    /// The store's side of `met`, its board taken from `alloc` now (rule 75).
    pub fn of(met: AsksMet, alloc: Alloc<'static>) -> Result<Asks, ClaimError> {
        Ok(match met {
            AsksMet::Again => Asks::Again,
            AsksMet::Once(plant) => Asks::Once { board: Board::new_in(BOARD_SLOTS, 1, alloc)?, plant, refused: RefCell::new(None) },
        })
    }

    pub fn once(&self) -> bool {
        matches!(self, Asks::Once { .. })
    }

    /// How many times `BodyWorld::again` asks one question at most: each pass past the first
    /// made at least one fact, and a store makes at most a board of them under `Once`.
    pub fn most_asks_again(&self) -> usize {
        match self {
            Asks::Again => super::world_items::MOST_IMPORTS_A_QUESTION,
            Asks::Once { board, .. } => board.table().capacity() as usize,
        }
    }

    /// The refusal an ask of this store kept, taken.
    pub fn take_refused(&self) -> Option<AskRefused> {
        match self {
            Asks::Again => None,
            Asks::Once { refused, .. } => refused.borrow_mut().take(),
        }
    }
}

impl Store<'_> {
    /// The facts made on the spot so far: the one count `BodyWorld::again` and check.rs's round
    /// loop watch. Under `Again`, `imported`; under `Once`, what the one ask made.
    pub fn imports(&self) -> u64 {
        match &self.asks {
            Asks::Again => self.imported.get(),
            Asks::Once { .. } => self.asked.borrow().made.iter().sum(),
        }
    }

    /// Whether a check over this store stops at its first hole (blaze-typeck
    /// `World::stops_at_holes`): under the one ask over frozen heads every fact that can be
    /// made inline is, so a hole is real and nothing runs past it (no stand-in is read), and the
    /// round's fills are made whole (check.rs `make_all`, no refusal dropped as speculative).
    /// THE ONE TEST of the new mode: world_items.rs and check.rs read it here.
    pub fn stops_at_holes(&self) -> bool {
        self.seeds.is_some() && self.asks.once()
    }

    /// An impl or inherent question answered on the spot under `Once` (check.rs
    /// `trait_question` / `inherent_question` filed its answer row): one fact made.
    pub(super) fn answered(&self, fact: Fact) {
        self.asked.borrow_mut().made[fact.index()] += 1;
    }
}

/// The need of a head want: `Members` for a trait's members, else `Head`, of the want's bytes
/// (tables/head.rs `want_bytes`, the frozen form's own order: kind, item, member).
pub fn need_of_want(want: Want) -> Need {
    let bytes = super::tables::head::want_bytes(want);
    let fact = if matches!(want, Want::Members(_)) { Fact::Members } else { Fact::Head };
    Need::whole(fact, Subject(ContentId::of(WANT_DOMAIN, &bytes)))
}

/// A `fmt::Write` that feeds a hasher: a question's `Debug` text hashed without a buffer.
struct Feed<'h>(&'h mut blaze_ids::Hasher);

impl core::fmt::Write for Feed<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.update(s.as_bytes());
        Ok(())
    }
}

/// The need of a checker question: its fact (tables/cost.rs `fact_of_kind`) and a subject
/// hashed from the question's `Debug` text (keys, shapes, names and indices only: no address
/// and no map order, rule 25). An impl question at one shape is a cell of the trait's bucket
/// (its detail non-zero), so it is late by design and never a gate's gap.
pub fn need_of_question(q: tc::Question) -> Need {
    let fact = super::tables::cost::fact_of_kind(tc::question_kind(q));
    let mut h = blaze_ids::Hasher::new(QUESTION_DOMAIN);
    // `Feed` never fails and a derived `Debug` fails only when its writer does.
    if write!(Feed(&mut h), "{q:?}").is_err() {
        unreachable!("a hasher's `fmt::Write` refused a question's text");
    }
    let subject = h.finish();
    let detail = match q {
        tc::Question::ImplsOf(..) => {
            let mut d = [0u8; 16];
            d.copy_from_slice(&subject.0[..16]);
            d[0] |= 1;
            Detail(d)
        }
        _ => Detail::WHOLE,
    };
    Need::new(fact, Subject(subject), detail)
}

/// The maker of a head need: its one step is the seed `Store::ensure` calls between rounds.
struct SeedMaker<'s, 'n> {
    store: &'s Store<'n>,
    keys: &'s Keys,
    seeds: &'s dyn HeadSeed<'n>,
    want: Want,
}

impl Maker for SeedMaker<'_, '_> {
    type Frame = ();

    fn start(&mut self, _: Need) {}

    fn step(&mut self, _: Need, _: &mut (), _: Option<Given>) -> Step {
        let _cost = super::tables::cost::Cost::enter_want(self.store.cost, self.want);
        let filed = self.seeds.seed(self.store, self.keys, self.want, self.store.tys.alloc());
        if matches!(filed, Ok(Seeded::Filed)) && self.store.made(self.want) { Step::Made(0) } else { Step::Failed(0) }
    }
}

/// `want` imported through the one ask of `store` (its board, under `AsksMet::Once`): made
/// once, read after. A refusal is kept on the store for the question that met it.
pub fn ask_seed<'n>(store: &Store<'n>, keys: &Keys, seeds: &dyn HeadSeed<'n>, want: Want) -> Result<Imported, AskRefused> {
    let need = need_of_want(want);
    let Asks::Once { board, plant, refused } = &store.asks else {
        return Err(AskRefused { need, why: "the store meets its heads again (`AsksMet::Again`); the one ask is the `Once` path" });
    };
    let mut maker = SeedMaker { store, keys, seeds, want };
    let mut asker: Asker<'_, Alloc<'static>, ()> = Asker::new(AskerOpts { board, me: ONE_TASK, alloc: store.tys.alloc(), most: blaze_ask::ask::MOST_DEPTH, plant: Plant::Clean });
    // One task: a publish of this store's asks wakes nobody.
    let got = asker.ask(&mut maker, need, FUEL, &mut |_| {});
    let counts: Counts = *asker.counts();
    store.asked.borrow_mut().add(&counts);
    let out = match got {
        Ok(Asked::Ready(Given::Made(_))) if counts.made[need.fact.index()] > 0 => match plant {
            AsksPlant::Clean => Ok(Imported::Made),
            AsksPlant::RefuseMade => Err(AskRefused { need, why: "PLANTED (AsksPlant::RefuseMade): a made head answered as refused" }),
        },
        Ok(Asked::Ready(Given::Made(_))) => Ok(Imported::Read),
        Ok(Asked::Ready(Given::Failed(_))) if counts.failed > 0 => Ok(Imported::Failed),
        Ok(Asked::Ready(Given::Failed(_))) => Ok(Imported::Read),
        Ok(Asked::Parked { .. }) => Err(AskRefused { need, why: "parked on another task's claim, on a board only this store's task claims on" }),
        Ok(Asked::Yielded) => Err(AskRefused { need, why: "out of fuel, for a seed that is one step" }),
        Err(error) => Err(AskRefused { need, why: refused_why(&error) }),
    };
    if let Err(r) = out {
        refused.borrow_mut().get_or_insert(r);
    }
    out
}

fn refused_why(error: &AskError<Alloc<'static>>) -> &'static str {
    match error {
        AskError::Cycle { .. } => "a cycle among the store's head needs",
        AskError::Abandoned { .. } => "its claim was abandoned",
        AskError::StackFull { .. } => "the work stack is at its bound (rule 18)",
        AskError::Word { .. } => "a maker step gave a word the board cannot carry",
        AskError::Table { why: ClaimError::Full { .. }, .. } => "the store's board is full (asks.rs `BOARD_SLOTS`, rule 18)",
        AskError::Table { .. } => "the claim table refused the claim",
        AskError::Busy { .. } => "the asker was busy with another need",
        AskError::Idle => "the asker was idle",
    }
}

/// A body's gate: off outside a check, collecting the round-0 prefetch, then its set.
pub enum Gate {
    Off,
    Collecting(AVec<'static, Need>),
    Set(GateSet<Alloc<'static>>),
}

impl Gate {
    /// A body's check starts: its gate collects the prefetch (round 0, before the walk).
    pub fn start(&mut self, alloc: Alloc<'static>) {
        *self = Gate::Collecting(AVec::new_in(alloc));
    }

    /// The prefetch's questions, while collecting (a later round's prefetch of its hints is
    /// the last round's reads, not the gate's).
    pub fn note(&mut self, questions: &[tc::Question]) {
        if let Gate::Collecting(needs) = self {
            needs.extend(questions.iter().map(|&q| need_of_question(q)));
        }
    }

    /// Round 0's walk is over: the set, sorted and each need once.
    pub fn freeze(&mut self, alloc: Alloc<'static>) {
        if let Gate::Collecting(needs) = core::mem::replace(self, Gate::Off) {
            *self = Gate::Set(GateSet::of(needs.iter().copied(), alloc));
        }
    }

    /// How `need`, met after entry, is classed: by the set once frozen; before (a body with a
    /// record, whose prefetch is its hints, or a first round not yet over) every item fact
    /// is the gate's gap, as no set names it.
    pub fn class(&self, need: &Need) -> After {
        match self {
            Gate::Set(set) => set.after_entry(need),
            Gate::Off | Gate::Collecting(_) if need.fact.gated(need.detail) => After::Gap,
            Gate::Off | Gate::Collecting(_) => After::Expected,
        }
    }

    /// The set's needs (empty until frozen).
    pub fn needs(&self) -> &[Need] {
        match self {
            Gate::Set(set) => set.needs(),
            Gate::Off | Gate::Collecting(_) => &[],
        }
    }
}
