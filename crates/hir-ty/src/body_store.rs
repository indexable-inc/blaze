//! The item store (docs/v2/plan/design/item-hash.md): a body's inference, MIR and borrow check
//! verdict filed in blaze's store under its identity and the facts the step read, so a later
//! build, branch or machine reads the result instead of running the step.
//!
//! - A body's step is filed under its **base key** (its identity from [`BodyFacts::identity`],
//!   the step, the schema of the step's answers, the records' schema and the solver version).
//!   Under it, in a slot of its chain, a **shape** lists the facts the step read; the
//!   **answer** is filed under the shape's bytes and those facts' values when it ran
//!   ([`record`]; the form, its keys and the chain are `blaze_record`'s). A lookup reads the
//!   shapes, hashes their facts as they are now, and asks for the answer: the goal store's
//!   constructive trace (`next_solver::goal_store`), one level up.
//! - A step is stored only when every entry of its read list (the recorder's,
//!   `blaze_memo::record`, through [`capture`]) is covered by the identity or stands for a
//!   fact ([`BodyFacts::fact_of`]); any other read keeps it out, counted by name in
//!   [`Flushed::refused`]. Only passing borrow checks are stored; under [`Mode::Trace`] the
//!   verdict is traced whatever it is ([`offer_verdict`]).
//! - Offers are buffered per thread and written by [`Installed::flush`], which the driver calls
//!   where it flushes the goal store, so no worker waits on the store ([`pending`]).
//! - Under [`Mode::Verify`] a hit runs the step as well, and a result that differs from the
//!   stored one is the build's error, naming the body and the step.
//! - Under [`Mode::Trace`] ([`install_trace`], a body task's store) every step runs, its trace
//!   is kept for the caller ([`Installed::trace`]), nothing is looked up and nothing is filed:
//!   the store is given no store of records, so it has none to read.
//! - Under [`Mode::Given`] ([`install_given`], a plan's snapshot) a body's inference, MIR and
//!   verdict are its body record's sections ([`GivenBodies`]), decoded in this database: items
//!   by their keys, types by content, positions as the body's own node ids. A body with no
//!   record, or no section of the step, is an [`Unserved`] refusal naming it, so the driver
//!   runs the body task; the body's text is never read in its place, and nothing is filed.
//!
//! - Each offer also keeps the run's trace (its facts and answer) per body, step and schema,
//!   which the kernel entries of the three steps read ([`entries`]): a carry then reaches
//!   exactly the bodies whose syntax record, identity or a fact changed.
//! - Inference, MIR and the borrow check ([`Step::served`]) are answered from the kernel's
//!   verified entries alone: the driver posts each entry it gets or carries
//!   ([`entries::serve`]), valid while the identity and every fact it read hash as they did
//!   (a kept record's validity is its read list), and the step's query reads the entry's
//!   identity, facts and earlier steps again on a hit. They offer nothing to
//!   the flush; the records above are a constant's value's until it has an entry.
//!
//! With no store installed for a database, every entry point is one load and a branch. A
//! record that does not open under its key or a store that cannot be read is an error naming
//! the body, reported by the next flush; the step is run then, which gives the same result. A
//! step the flush cannot keep (two answers under one key of fact values, a record the store
//! refuses) is named in what the flush did ([`Flushed::failure`]) and keeps no other step out.
//! A miss is a miss.

pub mod entries;
mod facts;
mod infer;
mod konst;
mod lower;
mod mir;
mod pending;
mod record;
#[cfg(test)]
mod tests;
mod value;

use std::{
    alloc::Global,
    cell::Cell,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use base_db::{Db, Nonce};
use blaze_core::lock::Lock;
use blaze_format::{Part, SchemaHash, schema_of};
use blaze_ids::ContentId;
use blaze_record::{AnswerKey, Bounds, Digest, Placed};
use hir_def::{DefWithBodyId, deps::ImageStore};

pub use facts::{BodyFacts, Classified, Step};
pub use lower::{Closure, Lowered, lowering};
pub(crate) use infer::inference;
pub(crate) use konst::constant;
#[cfg(test)]
use infer::lookup_inference;
pub(crate) use mir::built as built_mir;
pub(crate) use mir::given_closure_exprs;
pub(crate) use mir::{NONE as NO_PLACE, in_body, kind_code};
pub use mir::{calls_place_macro, is_place, location_literals, place_call, placed_literal};
#[doc(hidden)]
pub use mir::testing;
pub use crate::check_cache::ids::{closure_kind_code, closure_kind_of};
pub use value::step_value;

/// The inherent impls' associated types of `adt` a path's lowering looks a name up in
/// (`lower::path::inherent_assoc_types`), each by its binding name, alias and impl: what a
/// step's read of that query gave, for lane item-hash's fact of it
/// (`blaze_item_hash::values::inherent_assoc_types`), whose value is this list by item key.
pub fn inherent_assoc_types_of(
    db: &Db,
    adt: hir_def::AdtId,
) -> &[(hir_expand::name::Name, hir_def::TypeAliasId, hir_def::ImplId)] {
    crate::lower::path::inherent_assoc_types(db, adt)
}

use crate::next_solver::goal_store::{FactRef, slots, solver_version};
use crate::store_registry::{Linked, Registry};
use pending::{Counts, Offered, Pending, Tallies};

/// What a store does with a step: serve a stored answer, verify one, or only trace the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A hit is the step's result.
    Serve,
    /// A hit runs the step as well; a result that differs from the stored one is an error.
    Verify,
    /// No hit: every step runs, and its trace (`Kept { facts, scans, answer }` or `Refused`) is kept
    /// for the caller ([`Installed::trace`]); no stored answer is looked up, no served one is
    /// posted ([`entries::serve`] refuses) and nothing is filed ([`Installed::flush`]
    /// refuses). Installed by [`install_trace`], which takes no store of records: the store
    /// has none to read, by its type ([`Backing::Trace`]). The old cache's parts are not read
    /// either (`check_cache::served_body`).
    ///
    /// The invariant: in `Mode::Trace` no step's answer comes from anywhere but its own run in
    /// this process. Its control is `body_store::tests::under_trace_no_answer_comes_from_anywhere_but_its_run`
    /// (a planted served answer that differs from the run's: `Serve` answers with it, `Trace`
    /// with the run's) and hir-ty-borrowck's `trace_tests` (a planted old-cache pass of a body
    /// with an error).
    Trace,
    /// Every body's inference and MIR is its body record's ([`install_given`], [`given`]): a
    /// snapshot whose crates were all given from their surfaces, where a body's text may be
    /// absent. A step with no answer there is a typed refusal ([`Unserved`]) naming the body,
    /// so the driver runs the body task that makes the record; nothing runs in its place, and
    /// nothing is offered or filed.
    Given,
}

/// Where a store's answers are filed and found: a store of records in [`Mode::Serve`] and
/// [`Mode::Verify`], none in [`Mode::Trace`], the body records the driver gave in
/// [`Mode::Given`]. The records are reached only by matching [`Backing::Records`], so a trace
/// store cannot read one.
enum Backing {
    Records {
        // dyn: the driver's store over `blaze-cas`, probed once per step of a body, written at a flush
        images: Arc<dyn ImageStore + Send + Sync>,
        verify: bool,
    },
    Trace,
    // dyn: the driver's body records, one lookup per body step asked, above hir-ty; erased here
    // because the registry holds one store type for every mode (`install_given` takes a generic)
    Given(Box<dyn GivenBodies>),
}

/// The body records a snapshot is given ([`install_given`]): for a body, a step and the
/// step's answer schema, the answer's bytes in the body's record (its payload's section of
/// that step and schema: blaze-front's `body::payload`), or why there are none.
///
/// The invariant an implementation keeps: the bytes are those of the record the driver
/// checked under its key (`blaze_keyed::record::check_read`), and `fact` names that record's
/// result, so a query that took them lists what it read.
pub trait GivenBodies: Send + Sync + 'static {
    /// The answer of `step` of `def` under `schema`.
    fn answer(&self, db: &Db, def: DefWithBodyId, step: Step, schema: SchemaHash) -> Given;
}

/// What [`GivenBodies::answer`] found.
pub enum Given {
    /// The answer is `held.bytes()[range]`, 16-aligned, read in place; `fact` is what the
    /// asking query lists for it ([`GIVEN_ANSWER`]): the body's MIR fact
    /// (`blaze_keyed::record::mir_id`) and its record's result digest. `item` is the body's
    /// item key, for an error naming it.
    Answer { held: hir_def::deps::ImageBuf, range: std::ops::Range<usize>, fact: blaze_memo::Fact, item: ContentId },
    /// The body's record is given and has no section of this step and schema: an owner the
    /// body task makes no section for, or a step it did not run (the borrow check of a body
    /// made for its MIR alone).
    NotInRecord { item: ContentId },
    /// No record of the body is given: the driver runs its body task. `item` is the body's
    /// item key where it has one.
    NoRecord { item: Option<ContentId> },
    /// The body's record is given and does not open as one: a record the driver did not
    /// check under its key, or a payload of another format. Names why.
    Unreadable { item: ContentId, why: &'static str },
}

/// A step a [`Mode::Given`] store could not answer from the body's record, by the body, the
/// step, the item and why. Nothing runs in its place (rule 3): it unwinds out of the query that
/// asked, with this as its payload, as `hir_def::nameres::frozen::Unreadable` does, and the
/// driver that catches it runs the body task the record comes from ([`Unserved::NO_RECORD`])
/// or fails that one job with it (rule 20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unserved {
    pub def: DefWithBodyId,
    pub step: Step,
    /// The body's item key (`blaze_item_hash::values::body_key`), where it has one.
    pub item: Option<ContentId>,
    /// Which refusal: one of the `Unserved::*` constants.
    pub kind: &'static str,
    pub why: String,
}

impl Unserved {
    /// No body record of the body is given: the driver runs its body task, then asks again.
    pub const NO_RECORD: &'static str = "no body record of the body is given (its body task makes it)";
    /// The record is given and has no section of the step's schema: an owner a body task makes
    /// no section for (an anonymous constant inside the body, a by-move or `gen` coroutine).
    pub const NOT_IN_RECORD: &'static str = "the body record has no section of this step and schema";
    /// The section does not read in this database: a key that misses an input (rule 21).
    pub const UNREADABLE: &'static str = "the body record's section does not read in this database";
    /// The record was made for the body's MIR alone (no borrow check ran), and a borrow check's
    /// verdict was asked: an unchecked body is never taken as checked (rule 74).
    pub const UNCHECKED: &'static str = "the body record holds no borrow check verdict: its MIR is unchecked";
    /// The record's borrow check failed.
    pub const FAILED_CHECK: &'static str = "the body record's borrow check failed";
    /// The body's MIR holds location literals (`line!`, `file!` and friends), whose values are
    /// read off the body's text, which a given body does not have.
    pub const LOCATED: &'static str =
        "the body's MIR has location literals, which are filled from its text, and the body is given without it";

    fn of(def: DefWithBodyId, step: Step, item: Option<ContentId>, kind: &'static str, why: String) -> Unserved {
        Unserved { def, step, item, kind, why }
    }

    /// Unwinds with `self` as the payload, logged first.
    pub fn throw(self) -> ! {
        tracing::error!(target: "blaze", "{self}");
        std::panic::panic_any(self)
    }

    /// The `Unserved` a caught panic's payload is, if it is one.
    // dyn: a caught panic's payload, read once
    pub fn of_payload(payload: &(dyn std::any::Any + Send)) -> Option<&Unserved> {
        payload.downcast_ref::<Unserved>()
    }
}

impl fmt::Display for Unserved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the item store: the {} of {:?}", what(self.step), self.def)?;
        if let Some(item) = self.item {
            write!(f, " (item {item:x})")?;
        }
        write!(f, " is not served from a given body record: {}", self.kind)?;
        if !self.why.is_empty() {
            write!(f, ": {}", self.why)?;
        }
        Ok(())
    }
}

impl std::error::Error for Unserved {}

/// The kind of the input-like row a query lists for a given body record's answer it took
/// ([`Mode::Given`]): its fact is the body's MIR fact and the record's result digest, so a plan
/// that read the body moves when the record does.
pub const GIVEN_ANSWER: blaze_memo::KindId = blaze_memo::KindId::of(&GIVEN_ANSWER_KIND);

static GIVEN_ANSWER_KIND: blaze_memo::Kind =
    blaze_memo::Kind::new("hir_ty::body_store::given_answer", blaze_memo::CycleKind::Panic);

/// The domain of the digest of a served answer's bytes, in the fact a hit on it lists
/// ([`Store::lookup`]).
const SERVED_DOMAIN: &str = "blaze v2 item store served answer 1";

/// The schema of a borrow check's answer by the checker named `checker`: the verdict, empty
/// bytes for a pass, else the encoded refusal (`hir_ty_borrowck::gate::encode_verdict`). A
/// store of records holds only passes ([`offer_verdict`]); a trace holds either. Checkers
/// accept different programs, so each has its own key.
fn verdict_schema(checker: &'static str) -> SchemaHash {
    schema_of(&[Part::Str("blaze v2 item store borrow check verdict 2"), Part::Str(checker)])
}

/// `schema` for an answer about a body of `def`'s crate, with the crate attributes that change
/// what the body's MIR is, a part per attribute: `#![blaze::handles]` makes a move of a handle
/// that is used again a copy (`hir_ty_mir::mir::handles`), and `#![blaze::early_drop]` drops a
/// local whose destructor only frees memory after its last use (`hir_ty_mir::mir::early_drop`),
/// so the built MIR and the borrow check's verdict differ. A crate with neither keeps `schema`
/// itself, so its keys are what they were.
fn with_crate_attrs(db: &Db, def: DefWithBodyId, schema: SchemaHash) -> SchemaHash {
    use hir_def::HasModule;
    let def_map = hir_def::nameres::crate_def_map(db, def.krate(db));
    let schema = match def_map.handles() {
        true => schema_of(&[Part::Schema(schema), Part::Str("blaze::handles")]),
        false => schema,
    };
    match def_map.early_drop() {
        true => schema_of(&[Part::Schema(schema), Part::Str("blaze::early_drop 1")]),
        false => schema,
    }
}

/// What a step's name is in an error.
fn what(step: Step) -> &'static str {
    match step {
        Step::Infer => "inference",
        Step::Mir => "MIR",
        Step::Borrowck => "borrow check",
        Step::Const => "constant value",
        Step::Lower => "lowering",
    }
}

#[cfg(test)]
thread_local! {
    /// The planted bug of the hit report's test: the step whose hits are not told to the
    /// engine ([`Store::lookup`]), as a hit site left out would be.
    pub(crate) static UNREPORTED: Cell<Option<Step>> = const { Cell::new(None) };
}

/// Whether a test planted `step` as [`UNREPORTED`]: never, outside a test.
#[cfg(test)]
fn unreported(step: Step) -> bool {
    UNREPORTED.with(|it| it.get() == Some(step))
}

#[cfg(not(test))]
fn unreported(_step: Step) -> bool {
    false
}

/// What [`lookup`] found of a body's step.
pub(crate) enum Lookup<T> {
    /// No store for this database, or the body has no identity: run the step, offer nothing.
    Off,
    /// Nothing stored: run the step, then [`offer`] it with what it read.
    Miss,
    /// The stored result, decoded. With `verify`, run the step as well and [`fail`] when the
    /// two differ.
    Hit { value: T, verify: bool },
}

/// The item store of one database.
struct Store {
    nonce: Nonce,
    version: ContentId,
    backing: Backing,
    // dyn: lane item-hash's identities and facts, above hir-ty; memoized queries on a hit
    facts: Arc<dyn BodyFacts>,
    pending: Pending,
    /// Lookups since the last flush that found their base key's chain at its bound:
    /// [`Flushed::lookups_full`].
    full_lookups: AtomicUsize,
    /// What each step's lookups since the last flush came to, as `blaze_record` counted them,
    /// by [`Step`] as an index: [`Flushed::lookups`].
    lookups: [blaze_record::Tally; Step::COUNT],
    /// The first error a lookup met (a store that could not be read, a record that does not
    /// open under its key, a stored result that differs from running the step under
    /// [`Mode::Verify`]): [`Installed::flush`] stops with it.
    error: Lock<Option<String>>,
    /// The trace of the last run of each body's step in this database, by the body, the step
    /// and the schema it was offered under: what a kernel entry of the step reads
    /// ([`entries`]). A run of the step replaces it, so it is always of the run that made the
    /// value the database holds for the step now.
    traces: Lock<blaze_hash::HashMap<(DefWithBodyId, Step, SchemaHash), Arc<Traced>, std::alloc::Global>>,
    /// The kernel's verified answers of the served steps ([`Step::served`]) for this database,
    /// each valid while what it read hashes as it did ([`entries::serve`]): what
    /// [`Store::lookup`] answers those steps from.
    served: Lock<entries::Posted>,
}

/// Where a hit's answer bytes are: the kernel's verified entry, or the item store's record.
enum Answer {
    Served(Arc<[u8]>),
    Record(hir_def::deps::ImageBuf),
}

/// The buffers an offer fills, kept for the thread's next one. Taken out for the offer and put
/// back after, so an offer nested in one (a fact's value that infers another body) finds none
/// and allocates.
#[derive(Default)]
struct Scratch {
    facts: Vec<FactRef>,
    scans: Vec<(FactRef, &'static str)>,
    values: Vec<ContentId>,
    shape: Vec<u8>,
    answer: Vec<u8>,
}

thread_local! {
    static SCRATCH: Cell<Scratch> = Cell::new(Scratch::default());
}

/// Why an offered step is not kept, and so its trace is [`Traced::Refused`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A read of this query or input is neither covered by the identity nor a fact.
    Read(&'static str),
    /// A fact it read cannot be hashed now: the first such, by kind and subject.
    Unhashable(FactRef),
    /// The body has no identity.
    NoIdentity,
    /// What it read could not be captured shareably.
    Unshareable,
    /// Its result has no stored form.
    Unencodable,
}

impl Refusal {
    /// What a task the refusal fails is refused as: for a read, the name of its query or
    /// input's kind (`blaze_item_hash::reads::GENERIC_CONST_EVAL` and friends); else the
    /// reason, naming the rule.
    pub fn name(self) -> &'static str {
        match self {
            Refusal::Read(name) => name,
            Refusal::Unhashable(_) => "the item store: a fact the step read cannot be hashed now",
            Refusal::NoIdentity => "the item store: the body has no identity",
            Refusal::Unshareable => {
                "the item store: the step's read list was not recorded (a snapshot that keeps no read lists)"
            }
            Refusal::Unencodable => "the item store: the step's result has no stored form",
        }
    }
}

/// What the item store kept of the last run of a body's step in this database: what a kernel
/// entry of the step reads ([`entries`]) and what a body task takes ([`Installed::trace`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Traced {
    /// The facts the run read, sorted, each once, and the answer's bytes. The facts name the
    /// body's own item by lane item-hash's tag (`values::OWN`), never by an id; the answer
    /// names no `Gid` and no position.
    ///
    /// `scans` are the facts among `facts` read as a whole table ([`Classified::Scan`]), each
    /// with the name of the query that read it, in read order: what a reader that refuses such
    /// a fact names (rule 19), as `facts` holds no query.
    Kept { facts: Box<[FactRef]>, scans: Box<[(FactRef, &'static str)]>, answer: Box<[u8]> },
    /// The store would not keep the run, and why.
    Refused(Refusal),
}

/// Why [`Installed::trace`] has no trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Untraced {
    /// The store is not in [`Mode::Trace`]: a step's value may then be a stored answer, which
    /// its trace is not of.
    NotTracing,
    /// No run of the step under the schema asked left a trace: the step was not asked, or was
    /// asked under another schema.
    NotRun,
}

impl Untraced {
    /// What a task this fails is refused as.
    pub fn what(self) -> &'static str {
        match self {
            Untraced::NotTracing => {
                "the item store: a body task's trace from a store not in `Mode::Trace` (`install_trace`)"
            }
            Untraced::NotRun => "the item store: no run of the step under this schema left a trace",
        }
    }
}

impl Store {
    fn lookup<T>(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
        // dyn: the store's facts, given to the step's decoder once per hit, with the answer's
        // key (of a served step's answer, the zero key: it has none)
        decode: impl FnOnce(&dyn BodyFacts, &[u8], ContentId) -> Result<T, String>,
    ) -> Lookup<T> {
        let (images, verify) = match &self.backing {
            // `Mode::Trace`: nothing is looked up, so the step runs and its offer keeps its
            // trace. Neither the posted answers nor (there are none) records are read.
            Backing::Trace => {
                self.pending.looked_up(step, false);
                return Lookup::Miss;
            }
            Backing::Given(records) => return self.given(db, def, step, schema, &**records, decode),
            Backing::Records { images, verify } => (&**images, *verify),
        };
        let Some(identity) = self.facts.identity(db, def) else {
            self.pending.refused(step, Refusal::NoIdentity);
            self.pending.looked_up(step, false);
            return Lookup::Off;
        };
        // A served step's answer is the kernel's verified entry, whose reads hash as they did
        // (`entries::serve`); a constant's and a lowering's is the item store's own record
        // until it has one.
        let base = record::base_key(&identity, step, &schema, &self.version);
        let answer = match step.served() {
            true => self.served_answer(db, def, step, schema, identity).map(|served| {
                // clone: a counted reference to the posted answer
                let at = served.answer.clone();
                (Answer::Served(at), ContentId::default())
            }),
            false => self
                .find_record(images, db, def, step, &base)
                .map(|(found, key)| (Answer::Record(found), key)),
        };
        let found = match answer {
            None => Lookup::Miss,
            Some((found, answer_key)) => {
                let (opened, digest) = match &found {
                    Answer::Served(bytes) => (Ok(&bytes[..]), ContentId::of(SERVED_DOMAIN, bytes)),
                    Answer::Record(found) => {
                        (record::open_answer(found.bytes(), &answer_key), answer_key)
                    }
                };
                match opened.and_then(|body| decode(&*self.facts, body, answer_key)) {
                    Ok(value) => {
                        // The one hit site of the three steps. The running query's value is
                        // the record's, found by a key that stands for the reads of the run
                        // that made it: its read list holds those reads (the identity and
                        // each fact of the shape, read just above, each noted by the query
                        // that gave it) and the stored answer's own row, of the kind
                        // [`STORED_ANSWER`]: its fact is the base key and
                        // the digest of the answer it took (a record's answer key, a served
                        // answer's bytes), as the goal store lists its hits
                        // (`goal_store::Store::lookup`). Under `Mode::Verify` the step runs
                        // as well, in this query, by its own reads: nothing to note.
                        if !verify && !unreported(step) {
                            blaze_memo::read(
                                blaze_memo::Database::memo(db),
                                blaze_memo::Read::Input {
                                    kind: STORED_ANSWER,
                                    fact: blaze_memo::Fact { id: base, digest },
                                },
                            );
                        }
                        Lookup::Hit { value, verify }
                    }
                    Err(why) => {
                        self.fail(format_args!(
                            "the stored {} of {def:?} (answer {answer_key:x}): {why}",
                            what(step)
                        ));
                        Lookup::Miss
                    }
                }
            }
        };
        self.pending.looked_up(step, matches!(found, Lookup::Hit { .. }));
        found
    }

    /// [`Store::lookup`] in [`Mode::Given`]: the answer of `step` of `def` is the given body
    /// record's section, decoded, or a typed refusal ([`Unserved`]); no identity is computed and
    /// nothing runs in its place. A constant's value and a lowering are not in a body record:
    /// they run ([`Lookup::Off`]), the value from the given MIR, the lowering from the text,
    /// which a given body does not have (`hir_def`'s keyed-file refusal names it).
    ///
    /// The borrow check (rule 74): an empty section is a pass; a section with bytes is the
    /// record's failing verdict ([`Unserved::FAILED_CHECK`]); none is a body whose MIR was
    /// made unchecked ([`Unserved::UNCHECKED`]), never a pass.
    fn given<T>(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
        // dyn: as `Backing::Given`
        records: &dyn GivenBodies,
        // dyn: as `Store::lookup`
        decode: impl FnOnce(&dyn BodyFacts, &[u8], ContentId) -> Result<T, String>,
    ) -> Lookup<T> {
        let refuse = |item: Option<ContentId>, kind: &'static str, why: String| -> ! {
            Unserved::of(def, step, item, kind, why).throw()
        };
        if matches!(step, Step::Const | Step::Lower) {
            self.pending.looked_up(step, false);
            return Lookup::Off;
        }
        let (held, range, fact, item) = match records.answer(db, def, step, schema) {
            Given::Answer { held, range, fact, item } => (held, range, fact, item),
            Given::NotInRecord { item } => match step {
                Step::Borrowck => refuse(Some(item), Unserved::UNCHECKED, String::new()),
                // alloc: error path
                _ => refuse(Some(item), Unserved::NOT_IN_RECORD, format!("schema {:x}", schema.0)),
            },
            Given::NoRecord { item } => refuse(item, Unserved::NO_RECORD, String::new()),
            // alloc: error path
            Given::Unreadable { item, why } => refuse(Some(item), Unserved::UNREADABLE, String::from(why)),
        };
        let Some(bytes) = held.bytes().get(range.clone()) else {
            // clone: a range of two words, for the error
            refuse(Some(item), Unserved::UNREADABLE, format!("a section at {range:?}, past its record"))
        };
        if step == Step::Borrowck && !bytes.is_empty() {
            refuse(Some(item), Unserved::FAILED_CHECK, String::new());
        }
        match decode(&*self.facts, bytes, ContentId::default()) {
            Ok(value) => {
                // The running query's value is the record's: its read list holds the record's
                // fact, so what it made moves with the body record and nothing else. The
                // decode's own reads (the items it names, by their keys) are noted by the
                // queries that gave them.
                blaze_memo::read(
                    blaze_memo::Database::memo(db),
                    blaze_memo::Read::Input { kind: GIVEN_ANSWER, fact },
                );
                self.pending.looked_up(step, true);
                Lookup::Hit { value, verify: false }
            }
            Err(why) => refuse(Some(item), Unserved::UNREADABLE, why),
        }
    }

    /// The kernel's verified answer of `step` of `def` under `schema` for this database, if
    /// one was posted for its body ([`entries::serve`], by [`entries::BodyName`], in any
    /// snapshot) and it still holds: the body's `identity` (read by
    /// the caller) and each fact it and every earlier step it read saw (the row holds them
    /// all, `entries::serve`) are read again here, in the running query, so its read list
    /// holds what the answer depends on, with nothing run. A kept answer's validity is its
    /// read list (front-end-on-memo B, `Revision`): one whose identity or a fact differs now
    /// was verified of another world, and is a miss.
    fn served_answer(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
        identity: ContentId,
    ) -> Option<Arc<entries::Served>> {
        // By the body's name, never its id: a row posted in an earlier snapshot answers in
        // this one while what it read holds, and the id it was posted for may be dead.
        let name = entries::BodyName::of(db, def)?;
        let served = self.served.lock().get(name, step, schema)?;
        if entries::unchecked() {
            return Some(served);
        }
        if served.identity != identity {
            return None;
        }
        for &(fact, seen) in &served.facts[..] {
            let value = self.facts.value(db, def, fact).map_or(blaze_kernel::Seen::Gone, blaze_kernel::Seen::Is);
            if value != seen {
                return None;
            }
        }
        Some(served)
    }

    /// The answer record of the first shape of `base` whose facts hash the same now, and its
    /// answer key: of a step with no kernel entry yet (not [`Step::served`]: a constant's
    /// value, a lowering). The served steps have no lookup of the item store's records.
    fn find_record(
        &self,
        // dyn: the store's records, as `Backing::Records` holds them
        images: &(dyn ImageStore + Send + Sync),
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        base: &ContentId,
    ) -> Option<(hir_def::deps::ImageBuf, ContentId)> {
        // What `blaze_record::lookup` cannot say through a read's digest: a name in a shape
        // that is no fact. The shape is passed over, and the lookup is an error.
        let mut no_fact = None;
        let found = blaze_record::lookup(
            &self.records(images, step),
            Bounds::STANDARD,
            base,
            |name| match slots::fact_of(name) {
                Ok(fact) => self.facts.value(db, def, fact).map(Digest::Is),
                Err(why) => {
                    no_fact.get_or_insert(why);
                    None
                }
            },
        );
        if let Some(why) = no_fact {
            self.fail(format_args!("the shape of the {} of {def:?}: {why}", what(step)));
            return None;
        }
        match found {
            Ok(blaze_record::Found::Hit { answer, key, at: _ }) => Some((answer, key)),
            // A chain at its bound is counted here, and where the step's shape is then not
            // placed (`Flushed::slots_full`, which names the step).
            Ok(blaze_record::Found::Miss { lists: _, full }) => {
                if full {
                    self.full_lookups.fetch_add(1, Ordering::Relaxed);
                }
                None
            }
            Err(error) => {
                let slots::Refused { doing, why } = error.into();
                self.fail(format_args!("{doing} the {} of {def:?}: {why}", what(step)));
                None
            }
        }
    }

    /// The driver's store as `blaze_record` reads and writes the records of `step`: a shape
    /// under [`record::shape_at`] of its key, an answer under [`record::answer_at`] of its.
    fn records<'s>(
        &'s self,
        // dyn: as in `find_record`
        images: &'s (dyn ImageStore + Send + Sync),
        step: Step,
    ) -> slots::Records<'s, impl Fn(blaze_record::Kind, &ContentId) -> ContentId> {
        slots::Records {
            images,
            at: move |kind: blaze_record::Kind, key: &ContentId| match kind {
                blaze_record::Kind::List => record::shape_at(step, key),
                blaze_record::Kind::Answer => record::answer_at(step, key),
            },
            tally: &self.lookups[step as usize],
        }
    }

    fn offer(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
        reads: Option<Reads>,
        // dyn: the store's facts, given to the step's encoder once per offer
        write: impl FnOnce(&dyn BodyFacts, &mut Vec<u8>) -> Result<(), String>,
    ) -> Option<ContentId> {
        // `Mode::Given` runs no served step, so offers only what ran outside the records (a
        // constant's value): nothing of it is filed or traced.
        if let Backing::Given(_) = self.backing {
            return None;
        }
        let mut scratch = SCRATCH.take();
        scratch.facts.clear();
        scratch.scans.clear();
        scratch.values.clear();
        scratch.answer.clear();
        let offered = self.offered(db, def, step, schema, reads, write, &mut scratch);
        let (ran, key) = match offered {
            Ok(key) => (Ok((&scratch.facts[..], &scratch.scans[..], &scratch.answer[..])), key),
            Err(refusal) => {
                self.pending.refused(step, refusal);
                (Err(refusal), None)
            }
        };
        self.trace(def, step, schema, ran);
        SCRATCH.set(scratch);
        key
    }

    fn offered(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
        reads: Option<Reads>,
        // dyn: as in `offer`
        write: impl FnOnce(&dyn BodyFacts, &mut Vec<u8>) -> Result<(), String>,
        scratch: &mut Scratch,
    ) -> Result<Option<ContentId>, Refusal> {
        let identity = self.facts.identity(db, def).ok_or(Refusal::NoIdentity)?;
        let reads = reads.ok_or(Refusal::Unshareable)?;
        // The read list is the recorder's: each input the run read and each fact boundary
        // query it called, every other query flattened by the memo, so each row is asked once
        // and none is read through. An interned value is an identity, never listed. Only the
        // `FactRef`s `fact_of` gives are filed, never a row's `Gid` or kind.
        for &read in &reads[..] {
            match self.facts.fact_of(db, def, step, read) {
                Classified::Covered => {}
                Classified::Fact(fact) => scratch.facts.push(fact),
                Classified::Scan(fact, name) => {
                    self.pending.scanned(what(step), name);
                    scratch.facts.push(fact);
                    scratch.scans.push((fact, name));
                }
                Classified::Refused(name) => return Err(Refusal::Read(name)),
            }
        }
        scratch.facts.sort_unstable();
        scratch.facts.dedup();
        for &fact in &scratch.facts {
            scratch.values.push(self.facts.value(db, def, fact).ok_or(Refusal::Unhashable(fact))?);
        }
        if let Err(why) = write(&*self.facts, &mut scratch.answer) {
            tracing::debug!(?def, step = what(step), %why, "a step's result with no stored form");
            return Err(Refusal::Unencodable);
        }
        // A served step's record is the kernel's entry, made from this trace
        // ([`Store::trace`], read by `entries`' run of the step): nothing to flush.
        if step.served() {
            return Ok(None);
        }
        let base = record::base_key(&identity, step, &schema, &self.version);
        record::write_shape(&mut scratch.shape, &base, &scratch.facts);
        // The key the flush files the answer under (`Store::write`) and a lookup finds it by
        // (`blaze_record::Found::Hit`'s `key`): the shape, so the base key, then each fact's
        // value. What a lowering's fact is valued by (`Lowered::value`).
        let mut key = AnswerKey::new(&scratch.shape[..]);
        for &value in &scratch.values {
            key.value(Digest::Is(value));
        }
        let key = key.finish();
        // `Mode::Trace` files nothing: the key is still the step's value as a fact (a
        // lowering's, `Lowered::value`), which a later step's trace reads.
        if let Backing::Trace = self.backing {
            return Ok(Some(key));
        }
        self.pending.push(Offered {
            step,
            def,
            base: &base,
            shape: &scratch.shape,
            values: &scratch.values,
            answer: &scratch.answer,
        });
        Ok(Some(key))
    }

    /// Keeps what the run of `step` of `def` under `schema` just made as its trace
    /// ([`Traced`]), replacing the trace of the run before.
    fn trace(
        &self,
        def: DefWithBodyId,
        step: Step,
        schema: SchemaHash,
        ran: Result<(&[FactRef], &[(FactRef, &'static str)], &[u8]), Refusal>,
    ) {
        let ran = match ran {
            // alloc: one trace a body's step run, replacing the one before
            Ok((facts, scans, answer)) => {
                Traced::Kept { facts: facts.into(), scans: scans.into(), answer: answer.into() }
            }
            Err(refusal) => Traced::Refused(refusal),
        };
        // alloc: one trace a body's step, kept while the store is installed
        self.traces.lock().insert((def, step, schema), Arc::new(ran));
    }

    /// The trace of the last run of `step` of `def` under `schema` in this database.
    fn traced(&self, def: DefWithBodyId, step: Step, schema: SchemaHash) -> Option<Arc<Traced>> {
        // clone: a counted reference
        self.traces.lock().get(&(def, step, schema)).cloned()
    }

    #[cold]
    #[inline(never)]
    fn fail(&self, why: fmt::Arguments<'_>) {
        let mut error = self.error.lock();
        if error.is_none() {
            // alloc: error text, once per store until the flush that reports it
            *error = Some(format!("the item store: {why}"));
        }
    }

    /// Writes one buffered step: its shape in its base key's chain where it is held or the
    /// first slot is free (`filing`: the placing of this flush's shapes of the step's base
    /// key, `goal_store::slots::filing`), then its answer under the shape's bytes and the
    /// values its facts had.
    fn write<'a>(
        &self,
        // dyn: as in `find_record`
        images: &(dyn ImageStore + Send + Sync),
        entry: &pending::Entry<'a>,
        filing: &mut blaze_record::Filing<'a>,
    ) -> Result<Wrote, String> {
        let (step, def) = (entry.step, entry.def);
        // alloc: error text
        let error = |doing: &str, why: String| {
            format!("the item store: {doing} the {} of {def:?}: {why}", what(step))
        };
        let refused = |refused: blaze_record::Error<String>| {
            let slots::Refused { doing, why } = refused.into();
            error(doing, why)
        };
        let records = self.records(images, step);
        let wrote = match filing.place(&records, entry.shape).map_err(&refused)? {
            Placed::Written(_) => Wrote::Written,
            Placed::Kept(_) => Wrote::Kept,
            Placed::Full => return Ok(Wrote::SlotsFull),
        };
        let mut key = AnswerKey::new(entry.shape);
        for value in entry.values() {
            key.value(Digest::Is(value));
        }
        let answer_key = key.finish();
        // alloc: the answer's bytes, once per step written
        let answer = record::write_answer(&answer_key, entry.answer);
        match blaze_record::keep_answer(&records, answer_key, &answer) {
            Ok(blaze_record::Kept::Written) => Ok(Wrote::Written),
            Ok(blaze_record::Kept::Held) => Ok(wrote),
            Err(blaze_record::Error::TwoAnswers { key: _ }) => Err(error(
                "two answers under one key of fact values for",
                // alloc: error text
                String::from("the step read something its identity and facts do not cover"),
            )),
            Err(other) => Err(refused(other)),
        }
    }
}

/// What writing one buffered step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wrote {
    /// Its shape or its answer was new.
    Written,
    /// The store had both already.
    Kept,
    /// Every slot of its base key's chain holds another shape (`blaze_record::Placed::Full`).
    SlotsFull,
}

/// What [`Installed::flush`] did, and why offered steps were not kept.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Flushed {
    /// Steps whose shape or answer was written.
    pub written: usize,
    /// Steps the store had already.
    pub kept: usize,
    /// Steps not written as every slot of their base key's chain holds another shape: the
    /// chain's bound (`blaze_record::Bounds::STANDARD`), which a later lookup of the step
    /// misses on.
    pub slots_full: usize,
    /// Lookups that found their key's chain at its bound (`blaze_record::Found::Miss`'s
    /// `full`): what was run for one of them is among `slots_full` at the next flush, unless
    /// the chain holds its shape where a placing looks.
    pub lookups_full: usize,
    /// What each step's lookups of the store since the last flush came to, as `blaze_record`
    /// counted them, by [`Step`] as an index: how many there were (a body with no identity
    /// is not looked up), how many found an answer, the shapes they read and the facts they
    /// valued, each a fact hashed again. [`Flushed::steps`]' hits are the answers that also
    /// decoded.
    pub lookups: [blaze_record::Counts; Step::COUNT],
    /// The first, in the order the flush writes, of those counted in `slots_full`: which it
    /// is, by its key, and the bound.
    pub full: Option<String>,
    /// For each query or input whose read kept steps out (neither covered nor a fact), how
    /// many, most first: what lane item-hash's classification is asked to cover.
    pub refused: Vec<(&'static str, u64)>,
    /// Reads that took a whole table where a lookup would do ([`Classified::Scan`]: an
    /// iteration of a module's scope), by the step that read (as [`what`] names it) then the
    /// row read, with how many offers made each, most first: each is a fact that any change
    /// of the table changes, so who makes them is what to fix.
    pub scans: Vec<(&'static str, &'static str, u64)>,
    /// Steps kept out as a fact they read could not be hashed.
    pub unhashable: u64,
    /// Lookups of bodies with no identity, which are neither looked up nor stored.
    pub no_identity: u64,
    /// Steps kept out as what they read could not be captured shareably.
    pub unshareable: u64,
    /// Steps kept out as their result has no stored form.
    pub unencodable: u64,
    /// Steps dropped as the buffer of their thread was full.
    pub dropped: u64,
    /// Steps the store would not keep (a store that could not be read or refused a record, a
    /// second answer under one key of fact values): each is a bug in a key or in the store,
    /// and every other step of the flush was written all the same.
    pub failed: usize,
    /// Why the first of them, in the order the flush writes, was not kept, naming the body.
    pub failure: Option<String>,
    /// What each step's lookups found and its offers met since the last flush, by [`Step`] as an
    /// index.
    pub steps: [StepCounts; Step::COUNT],
}

/// What one step's lookups found and its offers met.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepCounts {
    /// Results read from the store; under [`Mode::Verify`] each also ran.
    pub hits: u64,
    /// Steps that ran as nothing was stored, or as the body has no identity.
    pub ran: u64,
    /// Steps that ran and were kept out of the store (any reason but no identity).
    pub refused: u64,
}

/// The installed item stores, one per database with one at once (see `crate::store_registry`).
static REGISTRY: Registry<Store> = Registry::new("ty.body_store.limbo");

/// Runs `f` on the item store of `db`, if one is installed.
#[inline]
fn with_store<R>(db: &Db, f: impl FnOnce(&Store) -> R) -> Option<R> {
    if REGISTRY.is_empty() {
        return None;
    }
    let nonce = db.nonce();
    REGISTRY.with(|it| it.nonce == nonce, f)
}

/// Before running `step` of `def`: its stored result, decoded by `decode` from the answer's
/// bytes, if a store is installed for `db` and every fact a shape of it lists hashes as it did.
/// The identity and the fact values are reads of the active query. `schema` is the schema of
/// the step's answers: another schema is another key.
#[inline]
pub(crate) fn lookup<T>(
    db: &Db,
    def: DefWithBodyId,
    step: Step,
    schema: SchemaHash,
    // dyn: as `Store::lookup`
    decode: impl FnOnce(&dyn BodyFacts, &[u8]) -> Result<T, String>,
) -> Lookup<T> {
    lookup_keyed(db, def, step, schema, |facts, bytes, _| decode(facts, bytes))
}

/// [`lookup`], with the key the answer was found under given to `decode` too: the digest of
/// the base key and of each fact's value the shape lists, which a lowering's fact is valued
/// by ([`Lowered::value`]). A served step's answer has none: the zero key.
#[inline]
pub(crate) fn lookup_keyed<T>(
    db: &Db,
    def: DefWithBodyId,
    step: Step,
    schema: SchemaHash,
    // dyn: as `Store::lookup`
    decode: impl FnOnce(&dyn BodyFacts, &[u8], ContentId) -> Result<T, String>,
) -> Lookup<T> {
    if REGISTRY.is_empty() {
        return Lookup::Off;
    }
    with_store(db, |store| store.lookup(db, def, step, schema, decode)).unwrap_or(Lookup::Off)
}

/// After a [`Lookup::Miss`]: `step` of `def` ran, reading `reads` (from [`capture`]), and
/// `write` puts its result's bytes in the buffer it is given, or refuses with why. Nothing is
/// written until the flush. The key the answer will be filed under, for a step the item
/// store files itself (not [`Step::served`]) and kept; `None` for a served step's run, a
/// refused one, or with no store.
#[inline]
pub(crate) fn offer(
    db: &Db,
    def: DefWithBodyId,
    step: Step,
    schema: SchemaHash,
    reads: Option<Reads>,
    // dyn: as `Store::offer`
    write: impl FnOnce(&dyn BodyFacts, &mut Vec<u8>) -> Result<(), String>,
) -> Option<ContentId> {
    if REGISTRY.is_empty() {
        return None;
    }
    with_store(db, |store| store.offer(db, def, step, schema, reads, write)).flatten()
}

/// Records the build's error for `step` of `def`: `why` it could not be served, or how its
/// stored result differs from running it under [`Mode::Verify`]. The next flush stops with the
/// first one.
#[cold]
pub(crate) fn fail(db: &Db, def: DefWithBodyId, step: Step, why: fmt::Arguments<'_>) {
    with_store(db, |store| store.fail(format_args!("the {} of {def:?}: {why}", what(step))));
}

/// A step's read list: the inputs its run read and the fact boundary queries it called,
/// sorted, each once (`blaze_memo::record`). Never filed as it is: [`BodyFacts::fact_of`]
/// turns each row into a [`FactRef`] first.
pub type Reads = Vec<blaze_memo::Read, Global>;

/// The kind of the input-like row a hit lists for the stored answer it took
/// ([`Store::lookup`]): its fact is the step's base key and the answer's digest, so
/// [`BodyFacts::fact_of`] tells it from a file's text by this kind. A kind of its own, as the
/// memo's reserved kinds are (`blaze_memo::AROUND_INPUTS`); its name hash orders it in a list.
pub const STORED_ANSWER: blaze_memo::KindId = blaze_memo::KindId::of(&STORED_ANSWER_KIND);

static STORED_ANSWER_KIND: blaze_memo::Kind =
    blaze_memo::Kind::new("hir_ty::body_store::stored_answer", blaze_memo::CycleKind::Panic);

/// `run`, with what it read recorded when an item store is installed for any database: what
/// [`offer`] and [`offer_passed`] are given. Only in a snapshot that keeps read lists
/// (`blaze_memo::Reads::Kept`) is anything recorded; in another, the offer is refused as
/// unshareable ([`Flushed::unshareable`]).
#[inline]
pub fn capture<R>(db: &Db, run: impl FnOnce() -> R) -> (R, Option<Reads>) {
    let memo = blaze_memo::Database::memo(db);
    if REGISTRY.is_empty() || !memo.keeps_reads() {
        return (run(), None);
    }
    let (value, reads) =
        blaze_memo::record(memo, run).expect("the recorder is on: `keeps_reads` said so above");
    (value, Some(reads))
}

/// Before `def`'s borrow check by the checker named `checker` (`hir_ty_borrowck::gate::checked`):
/// whether a passing verdict of it is stored, and so the check need not run. Under
/// [`Mode::Verify`] a stored verdict runs `check` (whether the borrow check passes) and answers
/// `false`, so the check runs as without a store; a stored pass whose check fails is the build's
/// error.
#[inline]
pub fn lookup_passed(
    db: &Db,
    def: DefWithBodyId,
    checker: &'static str,
    check: impl FnOnce() -> bool,
) -> bool {
    if REGISTRY.is_empty() {
        return false;
    }
    lookup_passed_in(db, def, checker, check)
}

#[inline(never)]
fn lookup_passed_in(
    db: &Db,
    def: DefWithBodyId,
    checker: &'static str,
    check: impl FnOnce() -> bool,
) -> bool {
    let schema = with_crate_attrs(db, def, verdict_schema(checker));
    // A store of records holds only passes ([`offer_verdict`]), and a trace store looks
    // nothing up (`Lookup::Miss`), so an answer with bytes is a store gone wrong.
    let found = lookup(db, def, Step::Borrowck, schema, |_, bytes| {
        match bytes.is_empty() {
            true => Ok(()),
            // alloc: the reason a record does not read
            false => Err(String::from("a borrow check's answer with bytes")),
        }
    });
    match found {
        Lookup::Off | Lookup::Miss => false,
        Lookup::Hit { value: (), verify: false } => true,
        Lookup::Hit { value: (), verify: true } => {
            if !check() {
                fail(db, def, Step::Borrowck, format_args!("stored as passed, and it fails"));
            }
            false
        }
    }
}

/// After `def`'s borrow check by the checker named `checker` passed, reading `reads` (from
/// [`capture`], around the check): its verdict, for the flush to store. A failing verdict is
/// never offered ([`offer_verdict`] with `keep`).
#[inline]
pub fn offer_passed(
    db: &Db,
    def: DefWithBodyId,
    checker: &'static str,
    reads: Option<Reads>,
) {
    offer_verdict(db, def, checker, reads, true, |_| {});
}

/// After `def`'s borrow check by the checker named `checker` ran, reading `reads` (from
/// [`capture`], around the check): its verdict, `write` putting its bytes in the buffer it is
/// given (`hir_ty_borrowck::gate::encode_verdict`: none for a pass). `keep` says whether a
/// store of records may keep it (it passed with nothing to say, `hir_ty_borrowck::kept`):
/// in [`Mode::Serve`] and [`Mode::Verify`] only such a verdict is offered, and its bytes are
/// none. In [`Mode::Trace`] it is offered whatever it is, so a body with a borrow error has a
/// trace, its answer the encoded errors, and its read list. With no store, nothing.
///
/// Floor: the encoding of the verdict, once a run of the check, and the read list's
/// classification the trace needs anyway; a pass writes no bytes.
#[inline]
pub fn offer_verdict(
    db: &Db,
    def: DefWithBodyId,
    checker: &'static str,
    reads: Option<Reads>,
    keep: bool,
    write: impl FnOnce(&mut Vec<u8>),
) {
    if REGISTRY.is_empty() {
        return;
    }
    if !keep && !tracing(db) {
        return;
    }
    let schema = with_crate_attrs(db, def, verdict_schema(checker));
    offer(db, def, Step::Borrowck, schema, reads, |_, out| {
        write(out);
        Ok(())
    });
}

/// Whether `db`'s item store is in [`Mode::Trace`]: every step runs and is traced, nothing is
/// looked up. False with no store. What the old cache's readers ask first, so under a trace
/// store they read no part (`check_cache::served_body`). A store is installed for a database
/// before any query of it runs and stays for its life, so this is no input of a query.
#[inline]
pub fn tracing(db: &Db) -> bool {
    if REGISTRY.is_empty() {
        return false;
    }
    with_store(db, |store| matches!(store.backing, Backing::Trace)).unwrap_or(false)
}

/// Gives `db` an item store: `images`, the driver's store (the local store, or the local store
/// over the global cache through `hir_def::deps::ReadThrough`), `facts`, lane item-hash's
/// identities and facts, `build`, the content ID of the front end's code (what the goal store is
/// given), and what a hit does. Until the guard is dropped, every lookup and offer of `db` goes
/// to it; what was offered and not flushed is dropped with it.
///
/// `build` must be the identity of all of the front end's code and of how it was built, as the
/// driver's is (blaze-cargo `serve/unit/deps.rs`, `front_end`, `blaze_codegen_code::FRONT_END`):
/// inference, MIR and borrow check results are a function of hir-ty, the vendored solver and
/// how they are built, all of which that identity is of, and nothing else in a key names them.
pub fn install(
    db: &Db,
    // dyn: the driver's store, kept by the item store (see `Backing::Records`)
    images: Arc<dyn ImageStore + Send + Sync>,
    // dyn: lane item-hash's facts, kept by the item store (see `Store::facts`)
    facts: Arc<dyn BodyFacts>,
    build: ContentId,
    mode: Mode,
) -> Result<Installed, String> {
    let verify = match mode {
        Mode::Serve => false,
        Mode::Verify => true,
        // alloc: error text
        Mode::Trace => {
            return Err(String::from(
                "the item store: `Mode::Trace` is installed by `install_trace`, which is given no store of records to read",
            ));
        }
        // alloc: error text
        Mode::Given => {
            return Err(String::from(
                "the item store: `Mode::Given` is installed by `install_given`, which is given the body records to answer from",
            ));
        }
    };
    link(db, Backing::Records { images, verify }, facts, build)
}

/// Gives `db` an item store in [`Mode::Given`]: a plan's snapshot, every crate given from its
/// surface. Each body's inference and built MIR is decoded from `records`' section of it,
/// its ids made in `db` from the items' keys by `facts` (`BodyFacts::def_of`), its types by
/// content; a body with no section is an [`Unserved`] refusal naming it, never a lowering of
/// its text. `facts` and `build` as [`install`]'s; `build` is part of no given answer (the
/// records' keys hold the tool), and is kept for the store's version alone.
///
/// The one query a plan step asks of a body: `providers::mir_at(db, MirOwner::Body(
/// InferBodyId::DefWithBodyId(def)), MirStage::Built)` (`mir_body_query`), and of a closure
/// of it `MirOwner::Closure(id)`, each answered by `mir::built` from the record.
///
/// Floor (an estimate, rule 67): one `GivenBodies::answer` (a binary search over the given
/// records and one over the record's sections) plus the section's bytes read in place, and the
/// items it names made once per snapshot (each a `def_of` of its key, memoized by the query
/// that makes it). Above it: the item table of each answer is decoded on every ask that is
/// not memoized, which the built-MIR query is, so once per owner per snapshot.
pub fn install_given<G: GivenBodies>(
    db: &Db,
    // dyn: lane item-hash's facts, kept by the item store (see `Store::facts`)
    facts: Arc<dyn BodyFacts>,
    build: ContentId,
    records: G,
) -> Result<Installed, String> {
    // Boxed once per installed store, freed with it through the registry's limbo.
    link(db, Backing::Given(Box::new_in(records, std::alloc::Global)), facts, build)
}

/// Whether `db`'s item store is in [`Mode::Given`]: a body's inference and MIR come from its
/// record, and its text may be absent. What a reader that would open a body's store to check
/// an id asks first (`check_cache::ids`' closure check, the interned closures' debug
/// assertions). False with no store. A store is installed before any query of its database
/// runs and stays for its life, so this is no input of a query.
#[inline]
pub fn given(db: &Db) -> bool {
    if REGISTRY.is_empty() {
        return false;
    }
    with_store(db, |store| matches!(store.backing, Backing::Given(_))).unwrap_or(false)
}

/// Gives `db` an item store in [`Mode::Trace`]: a body task's. Every step of a body runs, its
/// trace is kept for [`Installed::trace`], and nothing is looked up or filed: it is given no
/// store of records, so it has none to read. `facts` and `build` as [`install`]'s.
///
/// The database must keep read lists (`blaze_memo::Reads::Kept`): in one that does not, every
/// trace is [`Traced::Refused`] with [`Refusal::Unshareable`].
pub fn install_trace(
    db: &Db,
    // dyn: lane item-hash's facts, kept by the item store (see `Store::facts`)
    facts: Arc<dyn BodyFacts>,
    build: ContentId,
) -> Result<Installed, String> {
    link(db, Backing::Trace, facts, build)
}

fn link(
    db: &Db,
    backing: Backing,
    // dyn: as in `install`
    facts: Arc<dyn BodyFacts>,
    build: ContentId,
) -> Result<Installed, String> {
    let nonce = db.nonce();
    if REGISTRY.with(|it| it.nonce == nonce, |_| ()).is_some() {
        // alloc: error text
        return Err(String::from("this database has an item store installed already"));
    }
    // alloc: setup: once per installed store, freed through the registry's limbo when it is
    // uninstalled
    let store = Box::new(Store {
        nonce,
        version: solver_version(&build),
        backing,
        facts,
        pending: Pending::new(),
        full_lookups: AtomicUsize::new(0),
        lookups: [const { blaze_record::Tally::new() }; Step::COUNT],
        error: Lock::new("ty.body_store.error", None),
        traces: Lock::new("ty.body_store.traces", blaze_hash::HashMap::new_in(std::alloc::Global)),
        served: Lock::new("ty.body_store.served", entries::Posted::default()),
    });
    Ok(Installed { linked: REGISTRY.link(store) })
}

/// An item store installed for a database ([`install`]); dropping it uninstalls it.
pub struct Installed {
    linked: Linked<Store>,
}

impl Installed {
    fn store(&self) -> &Store {
        self.linked.get()
    }

    /// What the store does with a step.
    pub fn mode(&self) -> Mode {
        match self.store().backing {
            Backing::Records { verify: false, .. } => Mode::Serve,
            Backing::Records { verify: true, .. } => Mode::Verify,
            Backing::Trace => Mode::Trace,
            Backing::Given(_) => Mode::Given,
        }
    }

    /// The trace of the last run of `step` of `def` under `schema` (the schema the step's
    /// entry point gave: `entries::run_infer`, `entries::run_mir`, `entries::borrowck_schema`)
    /// in this database: the facts it read and its answer's bytes, or why the store refused
    /// it. Only of a store in [`Mode::Trace`], where the step's value is its run's: in another
    /// mode it may be a stored answer, which no trace is of ([`Untraced::NotTracing`]).
    ///
    /// The body is named by its `DefWithBodyId`, which the caller has from the opened
    /// surface's index by stable id: no offset or position names it here.
    ///
    /// Floor: one lock and one map probe, a counted reference; nothing is copied.
    pub fn trace(&self, def: DefWithBodyId, step: Step, schema: SchemaHash) -> Result<Arc<Traced>, Untraced> {
        let store = self.store();
        if !matches!(store.backing, Backing::Trace) {
            return Err(Untraced::NotTracing);
        }
        store.traced(def, step, schema).ok_or(Untraced::NotRun)
    }

    /// What each step's lookups found and its offers met since the last flush, by [`Step`] as an
    /// index; the next flush's [`Flushed::steps`] has them too.
    pub fn steps(&self) -> [StepCounts; Step::COUNT] {
        self.store().pending.steps()
    }

    /// The first error a lookup met since the last flush or call (a store that could not be
    /// read, a record that does not open under its key, a stored result that differs from
    /// running the step), taken: the driver calls it once a build has ended, so the build it
    /// happened in stops with it.
    pub fn errors(&self) -> Result<(), String> {
        match self.store().error.lock().take() {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }

    /// Writes every step offered since the last flush, on the store's threads
    /// (`ImageStore::for_each`): the steps of one base key one after another, in an order that
    /// is of their bytes alone, so two shapes of one base key (two bodies of one identity
    /// whose steps read different facts) take two slots whichever thread offered them
    /// (`goal_store::slots`). The driver calls it where it flushes the goal store: after a
    /// build's front-end work, with no query running. An error a lookup met since the last
    /// flush is an error naming the body. A step the store will not keep (a record it
    /// refuses, a second answer under one key of fact values: a read the identity and facts
    /// do not cover) is counted and named in [`Flushed::failed`] and [`Flushed::failure`],
    /// and keeps no other step out: one record's refusal is not the loss of a build's.
    ///
    /// A store in [`Mode::Trace`] files nothing, so a flush of one is an error naming why.
    pub fn flush(&self) -> Result<Flushed, String> {
        let store = self.store();
        let images = match &store.backing {
            Backing::Records { images, .. } => images,
            // alloc: error text
            Backing::Trace => {
                return Err(String::from(
                    "the item store: a store in `Mode::Trace` files nothing, so it has nothing to flush (a body task takes each step's trace, `Installed::trace`)",
                ));
            }
            // alloc: error text
            Backing::Given(_) => {
                return Err(String::from(
                    "the item store: a store in `Mode::Given` answers from the body records it was given and files nothing",
                ));
            }
        };
        let (taken, tallies) = store.pending.take();
        if let Some(why) = store.error.lock().take() {
            return Err(why);
        }
        // alloc: flush, once per build: one entry per step offered
        let entries: Vec<pending::Entry<'_>> = taken.iter().flat_map(pending::entries).collect();
        // alloc: flush, as above
        #[expect(
            clippy::disallowed_types,
            reason = "body write results, set once per entry in this flush: not global"
        )]
        let results: Vec<std::sync::OnceLock<Result<Wrote, String>>> =
            entries.iter().map(|_| std::sync::OnceLock::new()).collect();
        let (order, runs) = slots::groups(
            entries.len(),
            |a, b| entries[a].cmp_bytes(&entries[b]),
            |a, b| entries[a].base == entries[b].base,
        );
        images.for_each(runs.len(), &|run| {
            // clone: a range of two words
            let run = &order[runs[run].clone()];
            // A run is the entries of one base key, and never empty.
            let mut filing = slots::filing(entries[run[0]].base);
            for &at in run {
                _ = results[at].set(store.write(&**images, &entries[at], &mut filing));
            }
        });
        let Tallies { refused, scans, counts } = tallies;
        let Counts { unhashable, no_identity, unshareable, unencodable, dropped, steps } = counts;
        let mut flushed = Flushed {
            lookups_full: store.full_lookups.swap(0, Ordering::Relaxed),
            lookups: std::array::from_fn(|step| store.lookups[step].take()),
            refused,
            scans,
            unhashable,
            no_identity,
            unshareable,
            unencodable,
            dropped,
            steps,
            ..Flushed::default()
        };
        let mut results = results;
        // In the order written, so the failure named is the same one whichever thread wrote it.
        for at in order {
            let wrote = results[at].take().unwrap_or_else(|| {
                panic!("`ImageStore::for_each` did not run every task of an item store flush")
            });
            match wrote {
                Ok(Wrote::Written) => flushed.written += 1,
                Ok(Wrote::Kept) => flushed.kept += 1,
                Ok(Wrote::SlotsFull) => {
                    flushed.slots_full += 1;
                    if flushed.full.is_none() {
                        let entry = &entries[at];
                        // alloc: once per flush that met a full chain
                        flushed.full = Some(format!(
                            "the {} of {:?} (base key {:x}), whose chain holds {} other shapes",
                            what(entry.step),
                            entry.def,
                            entry.base,
                            slots::full_chain()
                        ));
                    }
                }
                Err(why) => {
                    flushed.failed += 1;
                    flushed.failure.get_or_insert(why);
                }
            }
        }
        Ok(flushed)
    }
}
