//! A test binary as blaze-testsel selects its tests (docs/v2/plan/design/test-selection.md):
//! its execution table, made beside its link and kept with its record ([`table`]), the
//! [`Image`] selection reads it as ([`image`]), what a test executed as a [`Record`]
//! ([`record`]), each suite's index head and the binary it was filed at for the daemon's life
//! ([`Selectors`]), and the store as the selection's records ([`Filed`]).

use std::alloc::Global;
use std::sync::Arc;

use blaze_cas::{Kind, Store};
use blaze_codegen::{ExecTable, InstanceKey, JumpTable, driver::Built};
use blaze_core::lock::Lock;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    kind::{Key, Kind as _, Namespace, Tag, TestIndex, TestPostings},
    store::Put,
};
use blaze_testsel::{Bound, FnId, Image, Record, Version, host::Records};
use blaze_usercode::test::{Executed, wire::Table};
use hir_ty_codegen::v2::RootTest;

use crate::serve::work::RunPuts;

/// The execution table of the test binary `binary`, linked from `built` with the span tables
/// `spans` (`LinkInputs::spans`): `jump` is the jump table its link made (its identity, its
/// capacity and the slots given out), `slot_of` the slot its link named for a function,
/// `beside` the content of the code it runs that is in no table (zero for none), and `tests`
/// its harness's tests in order. An error names the binary: its tests can't be selected, and
/// are never all run in its place.
pub(crate) fn table(
    binary: &str,
    built: &Built,
    spans: &[(ContentId, Vec<u8>)],
    jump: JumpTable,
    slot_of: impl Fn(&InstanceKey) -> Option<u32>,
    beside: ContentId,
    tests: &[RootTest],
) -> Result<ExecTable, String> {
    // clone: once per test binary linked, the names the table keeps
    let tests = tests.iter().map(|it| (it.name.clone(), it.root));
    // alloc: error path
    ExecTable::new(built, spans, jump, slot_of, beside, tests)
        .map_err(|why| format!("the execution table of `{binary}`: {}", why.describe(built)))
}

/// `table` as selection reads it. A slot an incremental link emptied ([`ExecTable::EMPTY`]) is
/// given as it is: a slot is never dropped, as a record names slots by place. The jump table
/// it is of is named as the binary's runtime names it ([`Table`]), which a record is read
/// only against.
pub(crate) fn image(binary: &str, table: &ExecTable) -> Result<Image, String> {
    let mut fns = Vec::with_capacity_in(table.fns.len(), Global);
    fns.extend(table.fns.iter().map(|(id, bound)| Version { id: FnId(*id), bound: Bound(*bound) }));
    let mut slots = Vec::with_capacity_in(table.slot_fn.len(), Global);
    slots.extend_from_slice(&table.slot_fn);
    let jump = &table.jump;
    let of = Table { lineage: jump.lineage, generation: jump.generation, slots: jump.capacity };
    Image::new(fns, slots, table.beside, of)
        // alloc: error path
        .map_err(|why| format!("the execution table of `{binary}`: {why}"))
}

/// What a test's fork server says it executed, as selection keeps it.
pub(crate) fn record(executed: Executed) -> Record {
    match executed {
        Executed::Hits { table, hits } => Record::of_hits(table, hits),
        Executed::Not(why) => Record::Incomplete { why },
    }
}

/// A suite's selection state between requests: its index head (the root the machine's
/// `Start::head` is), the postings it names, and the program state its rows hold at, with
/// the image of that state when this daemon made it, which answers the changes since.
pub(crate) struct Held {
    /// The binary's content, when this daemon made `image` of it; none for a head restored
    /// from its journal mark ([`Selectors::restore`]).
    pub(crate) binary: Option<ContentId>,
    /// The program state (`Image::state`) the head was filed at.
    pub(crate) state: ContentId,
    pub(crate) image: Option<Arc<Image>>,
    pub(crate) head: Option<Key<TestIndex>>,
    pub(crate) postings: Option<ContentId>,
}

/// What a request starts a suite's selection from ([`Selectors::take`]).
pub(crate) struct Taken {
    /// The binary's image now.
    pub(crate) now: Arc<Image>,
    /// The image the head was filed at, when it is another binary's.
    pub(crate) before: Option<Arc<Image>>,
    pub(crate) head: Option<Key<TestIndex>>,
    /// What was held, given back as it was when the request does not end.
    pub(crate) held: Option<Held>,
}

/// The daemon's suites' heads, one per suite (its given): each test run takes its suite's
/// ([`Selectors::take`]) and gives the new head back when its selection ends
/// ([`Selectors::give`]); one that does not gives back what it took. The lock is taken twice
/// per suite per run, never per test.
///
/// A head outlives the daemon through its journal mark (`serve::local::TestHeadMark`, written
/// once its puts are durable, `Jobs::test_heads`), and the store keeps it through the pin
/// `collect::TEST_HEADS` while the mark names it. A start restores each mark
/// ([`Selectors::restore`]) with no image: the head is used for a binary of the state it was
/// filed at, and another binary's is a miss by key (each test looked up by its answers), as
/// the changes since a state with no image can't be told.
pub(crate) struct Selectors(Lock<HashMap<ContentId, Held, Global>>);

impl Default for Selectors {
    fn default() -> Selectors {
        Selectors(Lock::new("serve.test.selectors", HashMap::default()))
    }
}

impl Selectors {
    /// The suite `given`'s head and images for its binary `binary`: the image held when the
    /// binary is the one held, else made by `image`, with the held one as the one before.
    pub(crate) fn take(
        &self,
        given: ContentId,
        binary: ContentId,
        image: impl FnOnce() -> Result<Image, String>,
    ) -> Result<Taken, String> {
        // An insert or a remove that panicked left the map as it was: its value stands.
        let held = self.0.lock().remove(&given);
        let Some(held) = held else {
            return Ok(Taken { now: Arc::new(image()?), before: None, head: None, held: None });
        };
        if let (Some(at), Some(kept)) = (held.binary, &held.image)
            && at == binary
        {
            // clone: a reference count, once per suite per run
            return Ok(Taken { now: kept.clone(), before: None, head: held.head, held: Some(held) });
        }
        let now = Arc::new(image()?);
        let (before, head) = match &held.image {
            _ if now.state() == held.state => (None, held.head),
            // clone: a reference count, once per suite per run
            Some(kept) => (Some(kept.clone()), held.head),
            // A head restored from its mark, of another binary: no image of its state is
            // held, so its changes can't be told and it is not used. A miss by key.
            None => (None, None),
        };
        Ok(Taken { now, before, head, held: Some(held) })
    }

    /// Keeps `held` for the next run of the suite `given`.
    pub(crate) fn give(&self, given: ContentId, held: Held) {
        self.0.lock().insert(given, held);
    }

    /// Each suite's head its journal mark names, as a daemon's start reads them: held with no
    /// image, until a run takes it ([`Selectors::take`]). A suite this daemon holds a head of
    /// already keeps its own.
    pub(crate) fn restore(&self, marked: impl Iterator<Item = crate::serve::local::TestHeadMark>) {
        let mut held = self.0.lock();
        for mark in marked {
            held.entry(mark.given).or_insert(Held {
                binary: None,
                state: mark.state,
                image: None,
                head: Some(Key::from_id(mark.head)),
                postings: mark.postings,
            });
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.0.lock().len()
    }
}

/// The domain of a selection record's key in the store.
const RECORD_DOMAIN: &str = "blaze-cargo test selection record 1";

/// The store as a selection's records: each record of `tag`, `key` under one store key of
/// both, a run's kind; each put's ticket kept with the run's puts, waited on as the job ends.
pub(crate) struct Filed<'a> {
    pub(crate) store: &'a mut Store,
    pub(crate) puts: &'a mut RunPuts,
}

fn store_key(tag: Tag, key: ContentId) -> blaze_cas::Key {
    let mut hasher = Hasher::new(RECORD_DOMAIN);
    hasher.update(&tag.0.to_le_bytes()).id(&key);
    blaze_cas::Key(hasher.finish())
}

/// The store keys of a suite's index head `head` and the postings it names: what the pin
/// `collect::TEST_HEADS` keeps while the suite's journal mark names them.
pub(crate) fn pinned(head: ContentId, postings: Option<ContentId>) -> impl Iterator<Item = blaze_cas::Key> {
    std::iter::once(store_key(TestIndex::TAG, head))
        .chain(postings.map(|postings| store_key(TestPostings::TAG, postings)))
}

impl Records for Filed<'_> {
    type Error = blaze_cas::Corrupt;

    fn get(&mut self, tag: Tag, key: ContentId) -> Result<Option<&[u8]>, blaze_cas::Corrupt> {
        Ok(self.store.get(&store_key(tag, key))?.map(|entry| entry.bytes))
    }

    /// The store is the daemon's own: `namespace` (every record's is the local one) is in the
    /// record's key already.
    fn put(&mut self, tag: Tag, key: ContentId, _namespace: Namespace, bytes: &[u8]) -> Result<Put, blaze_cas::Corrupt> {
        let at = store_key(tag, key);
        if let Some(held) = self.store.get(&at)? {
            return Ok(match held.bytes == bytes {
                true => Put::Same,
                false => Put::Other,
            });
        }
        match self.store.put(at, Kind::Run, bytes) {
            Ok(ticket) => {
                self.puts.kept([ticket]);
                Ok(Put::Written)
            }
            Err(_) => Ok(Put::Other),
        }
    }
}
