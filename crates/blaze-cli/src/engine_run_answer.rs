//! The engine's stored answer as the run path reads it (run.md, "Run and test read the engine's
//! answer", piece E2): [`EngineAnswer`] implements `run_ids.rs`'s two seams, [`Today`] (a name's
//! out, a cell's digest) and [`Answer`] (a named record's parts then its payload, and a body's
//! facts), over the rows and records `engine-check` stored. Nothing here builds the old request.
//!
//! WHERE EACH THING IS READ, every one a point read of the answer (nothing loads the whole of it):
//! - a name's out: the row's [`RowFacts::out`] (`blaze_engine::Answer::row_facts`, a header read);
//! - a cell's digest today: the cell [`blaze_engine::Answer::cell`] gives, when the row's
//!   `defines` (`LastAnswer::defines`) lists the cell;
//! - a record: the key the row names ([`Records`]: the page header's `record` for the stored
//!   answer, `Kept::records` for one read whole), opened with the reader the old answer uses
//!   (run_ids.rs `open_record`);
//! - a body: its row (key) and its FACTS RECORD (engine_body_facts.rs: the class and the covered
//!   item keys). A row with no facts record is `Checked::Absent`, never `Clean` (rule 3); a
//!   record the row names that the store does not hold or that does not read is an error naming
//!   the body. A constructor's "body" is its ADT's head task (`Signature`): the same facts, the
//!   trailer of that task's record ([`facts_of`]);
//! - a body's reads: BY NEED, at the call that steps it, from the stored rows in place
//!   (`BodyReads` for `blaze_engine::Answer`, `Answer::observations` folded into one proof);
//! - a file the check planned no body task of (a features-skipped target's): its place in the
//!   root (`Root::places`, engine_check.rs `PLACE_SKIPPED`), so the walk does not open it.

use blaze_drive::{Cell, Key, Name, Tool};
use blaze_engine::last::LastAnswer;
use blaze_engine::pages::Kept;
use blaze_engine::CellId;
use blaze_ids::ContentId;
use bstd::cx::Cx;

use crate::engine_answer::Stored;
use crate::engine_body_facts::Class;
use crate::run_ids::{Answer, BodyFacts, BodyReads, BodyScratch, Checked, Missing, Proof, ProofFold, RunIdsError, Seen, Today, Unread, cell_fact, open_record};
use crate::store::Store;

const REFUSED: &str = "a page of the stored answer is refused";

/// Where the record of a row is: the key the driver put it under.
pub(crate) trait Records {
    /// `name`'s record key; `Ok(None)`: the row has none (or there is no row). `Err`: the stored
    /// answer refused the page that holds it.
    fn record(&self, name: Name) -> Result<Option<Key>, &'static str>;

    /// Whether the file `file`'s place carries engine_check.rs `PLACE_SKIPPED`: reached only from
    /// a target its features skip, outlined, with no body task (run_ids.rs `Answer::skipped`).
    fn skipped(&self, file: Name) -> bool;
}

/// Whether `place` is a features-skipped target's file (engine_check.rs `PLACE_SKIPPED`).
fn place_skipped(place: Option<&blaze_engine::pages::FilePlace>) -> bool {
    place.is_some_and(|place| place.flags & crate::engine_check::PLACE_SKIPPED != 0)
}

/// The answer read in place: a row's record is in the records section of its own page (the page
/// `page_of` names), never found through a producer header shard (pages/headers.rs, D187).
impl Records for Stored<'_> {
    fn record(&self, name: Name) -> Result<Option<Key>, &'static str> {
        match crate::engine_ready_edit::page(self, name) {
            Ok(page) => Ok(page.and_then(|page| page.record(name))),
            Err(_) => Err(REFUSED),
        }
    }

    /// The root's place of the file (the root holds every planned file's place, D187).
    fn skipped(&self, file: Name) -> bool {
        place_skipped(self.root.places.get(&file))
    }
}

/// The answer read whole: its `records` column.
impl Records for Kept<'_> {
    fn record(&self, name: Name) -> Result<Option<Key>, &'static str> {
        Ok(self.records.get(&name).copied())
    }

    /// Its `places`: a decoded answer's are the root's (pages.rs `decode_at`); a request's own
    /// hold only the files it planned, and a file not there is not said to be skipped (the walk
    /// then refuses a body part with no row by name).
    fn skipped(&self, file: Name) -> bool {
        place_skipped(self.places.get(&file))
    }
}

/// The engine's stored answer as [`Today`] and [`Answer`]: `answer` the rows (from
/// `Stored::answer`, or the whole `Kept::answer`), `records` where each row's record is, `store`
/// the records' bytes.
pub(crate) struct EngineAnswer<'q, 'a, 's, 'm> {
    answer: &'q blaze_engine::Answer<'a>,
    records: &'q dyn Records,
    store: &'s mut dyn Store<'m>,
}

impl<'q, 'a, 's, 'm> EngineAnswer<'q, 'a, 's, 'm> {
    pub(crate) fn new(answer: &'q blaze_engine::Answer<'a>, records: &'q dyn Records, store: &'s mut dyn Store<'m>) -> Self {
        EngineAnswer { answer, records, store }
    }

    /// A page the stored answer refused while a point read answered `None`: stops the question
    /// that asked, naming `name` (rule 3: a refused page is never read as an absent row).
    fn refused(&self, name: Name) -> Result<(), RunIdsError<'static>> {
        match LastAnswer::refusal(self.answer) {
            Some(_) => Err(RunIdsError::EngineAnswer { name, what: REFUSED }),
            None => Ok(()),
        }
    }
}

impl Today for EngineAnswer<'_, '_, '_, '_> {
    /// The row's `out`.
    fn out(&self, name: Name) -> Option<ContentId> {
        self.answer.row_facts(name).ok().flatten().map(|row| row.out)
    }

    /// The row's `defines` entry for `cell`: its digest today. `None`: no row, or it defines no
    /// such cell.
    fn cell(&self, name: Name, cell: Cell) -> Option<ContentId> {
        let id = CellId { of: name, cell };
        let mut defines = LastAnswer::defines(self.answer, name)?;
        defines.any(|(defined, _)| defined == id).then(|| self.answer.cell(&id))
    }
}

impl Answer for EngineAnswer<'_, '_, '_, '_> {
    fn open(&mut self, cx: &Cx, name: Name, seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>> {
        let record = self.records.record(name).map_err(|what| RunIdsError::EngineAnswer { name, what })?;
        let Some(at) = record else {
            // No record: no row, or a row that keeps none; a refused page is told, not skipped.
            self.refused(name)?;
            return Ok(false);
        };
        open_record(cx, &mut *self.store, name, at, seen)
    }

    fn skipped(&self, file: Name) -> bool {
        self.records.skipped(file)
    }

    fn body(
        &mut self,
        cx: &Cx,
        check: Name,
        _tool: Tool,
        _known: Option<(ContentId, Name)>,
        out: &mut BodyScratch<'_>,
    ) -> Result<Option<BodyFacts>, RunIdsError<'static>> {
        out.clear();
        let row = match self.answer.row_facts(check) {
            Ok(Some(row)) => row,
            Ok(None) => {
                self.refused(check)?;
                return Ok(None);
            }
            Err(_) => return Err(RunIdsError::EngineAnswer { name: check, what: REFUSED }),
        };
        // What `WorkspaceBody` holds besides the item: the row's source is the body's entry text
        // and its file the `FileItems` that lists it.
        let held = BodyFacts { key: row.key.0, checked: Checked::Absent(Missing::NoRecord), text: row.source, file: row.file };
        let record = self.records.record(check).map_err(|what| RunIdsError::EngineAnswer { name: check, what })?;
        // A row with no facts record: never `Clean`.
        let Some(at) = record else { return Ok(Some(held)) };
        let bytes = self.store.get(cx, at).map_err(|why| RunIdsError::AnswerStore { name: check, why })?;
        let Some(bytes) = bytes else { return Err(RunIdsError::AnswerGone { name: check }) };
        // The facts are the record's PAYLOAD (the Bodies stage puts a plain drive record: its
        // head, then the facts bytes; engine_check.rs).
        let whole = match blaze_drive::record::open(bytes.bytes).map_err(|why| RunIdsError::AnswerRecord { name: check, why })? {
            blaze_drive::record::Stored::Whole(whole) => whole,
            blaze_drive::record::Stored::Shape(_) => return Err(RunIdsError::EngineBody { check, what: "its facts record is a shape record, which holds no payload" }),
        };
        let facts = facts_of(check, whole.payload).map_err(|what| RunIdsError::EngineBody { check, what })?;
        let facts = crate::engine_body_facts::read(facts).map_err(|_| RunIdsError::EngineBody { check, what: "its facts record does not read (engine_body_facts.rs)" })?;
        let checked = match facts.class {
            Class::Clean => Checked::Clean,
            // At least one error by its type: a zero count was refused where the record was read
            // (engine_body_facts.rs `BodyFactsError::NoErrors`; rule 74).
            Class::Errors { errors, first } => Checked::Errors { errors, first },
            // Types whole, borrows not checked: run (D151, `Checked::runs`).
            Class::Typed { rule } => Checked::Typed { rule },
            Class::Untyped { rule } => Checked::Incomplete { rule },
        };
        out.covers.extend(facts.covered());
        // Its reads are not copied here: its check's proof is read by need at the call
        // (`BodyReads` below, run_ids.rs `RunIds::runnable`).
        Ok(Some(BodyFacts { checked, ..held }))
    }
}

/// The run facts' bytes (engine_body_facts.rs's form) in the payload of `check`'s record, the one
/// place that says where they are: a `BodyCheck`'s record IS its facts; a head task's
/// (`Signature`) holds them after its runs (engine_seeds.rs `runs_from`, engine_heads.rs
/// `ctor_facts`). A head payload with no trailer is refused by name.
pub(crate) fn facts_of(check: Name, payload: &[u8]) -> Result<&[u8], &'static str> {
    match check.kind {
        blaze_drive::Kind::Signature => crate::engine_seeds::runs_from(payload).map(|(_, facts)| facts),
        _ => Ok(payload),
    }
}

/// A workspace body's proof from the stored rows themselves (engine_check.rs `Standing::rows`),
/// read in place by need: the row's reads in its stored order, each a cell fact with the digest
/// the check saw and the cell's digest today, folded as they stream (run_ids.rs `ProofFold`),
/// nothing copied. A row the answer does not hold is `NoRow`, a refused page `Refused` (rule 3:
/// never read as an absent row), more than `most` reads `TooManyReads` (rule 18).
impl BodyReads for blaze_engine::Answer<'_> {
    fn proof(&self, check: Name, most: u32) -> Result<Proof, Unread> {
        let refused = |answer: &Self| LastAnswer::refusal(answer).is_some();
        let seen = match self.observations(check) {
            Ok(Some(seen)) => seen,
            Ok(None) if refused(self) => return Err(Unread::Refused { check }),
            Ok(None) => return Err(Unread::NoRow { check }),
            Err(_) => return Err(Unread::Refused { check }),
        };
        let count = seen.len();
        if count > most as usize {
            return Err(Unread::TooManyReads { check, count, most });
        }
        let mut fold = ProofFold::new(check);
        for (id, saw) in seen {
            fold.read(&cell_fact(id.of, id.cell), &saw, &self.cell(&id));
        }
        // A page refused while a digest was read answered `ABSENT` for it: never folded as one.
        if refused(self) {
            return Err(Unread::Refused { check });
        }
        Ok(fold.finish())
    }
}
