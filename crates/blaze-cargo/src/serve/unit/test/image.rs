//! A test binary as blaze-testsel selects its tests (docs/v2/plan/design/test-selection.md):
//! its execution table, made beside its link and kept with its record ([`table`]), the
//! [`Image`] selection reads it as ([`image`]), what a test executed as a [`Record`]
//! ([`record`]), each suite's index head and the binary it was filed at for the daemon's life
//! ([`Selectors`]), and the store as the selection's records ([`Filed`]).

use std::alloc::Global;
use std::sync::Arc;

use blaze_cas::{Kind, Store};
use blaze_codegen::{ExecTable, InstanceKey, driver::Built};
use blaze_core::lock::Lock;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    kind::{Key, Namespace, Tag, TestIndex},
    store::Put,
};
use blaze_testsel::{Bound, FnId, Image, Record, Version, host::Records};
use blaze_usercode::test::Executed;
use hir_ty_codegen::v2::RootTest;

use crate::serve::work::RunPuts;

/// The execution table of the test binary `binary`, linked from `built` with the span tables
/// `spans` (`LinkInputs::spans`): `slots` is how many
/// slots its jump table has, `slot_of` the slot its link named for a function, `beside` the
/// content of the code it runs that is in no table (zero for none), and `tests` its harness's
/// tests in order. An error names the binary: its tests can't be selected, and are never all
/// run in its place.
pub(crate) fn table(
    binary: &str,
    built: &Built,
    spans: &[(ContentId, Vec<u8>)],
    slots: u32,
    slot_of: impl Fn(&InstanceKey) -> Option<u32>,
    beside: ContentId,
    tests: &[RootTest],
) -> Result<ExecTable, String> {
    // clone: once per test binary linked, the names the table keeps
    let tests = tests.iter().map(|it| (it.name.clone(), it.root));
    // alloc: error path
    ExecTable::new(built, spans, slots, slot_of, beside, tests)
        .map_err(|why| format!("the execution table of `{binary}`: {}", why.describe(built)))
}

/// `table` as selection reads it. A slot an incremental link emptied ([`ExecTable::EMPTY`]) is
/// given as it is: a slot is never dropped, as a record names slots by place.
pub(crate) fn image(binary: &str, table: &ExecTable) -> Result<Image, String> {
    let mut fns = Vec::with_capacity_in(table.fns.len(), Global);
    fns.extend(table.fns.iter().map(|(id, bound)| Version { id: FnId(*id), bound: Bound(*bound) }));
    let mut slots = Vec::with_capacity_in(table.slot_fn.len(), Global);
    slots.extend_from_slice(&table.slot_fn);
    Image::new(fns, slots, table.beside)
        // alloc: error path
        .map_err(|why| format!("the execution table of `{binary}`: {why}"))
}

/// What a test's fork server says it executed, as selection keeps it.
pub(crate) fn record(executed: Executed) -> Record {
    match executed {
        Executed::Hits { slots, hits } => Record::of_hits(slots, hits),
        Executed::Not(why) => Record::Incomplete { why },
    }
}

/// A suite's selection state between requests: its index head (the root the machine's
/// `Start::head` is) and the binary the head's rows hold at, whose image answers the
/// changes since.
pub(crate) struct Held {
    /// The binary's content.
    pub(crate) binary: ContentId,
    pub(crate) image: Arc<Image>,
    pub(crate) head: Option<Key<TestIndex>>,
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

/// The daemon's suites' heads, one per suite (its given) for the daemon's life: each test run
/// takes its suite's ([`Selectors::take`]) and gives the new head back when its selection ends
/// ([`Selectors::give`]); one that does not gives back what it took. The lock is taken twice
/// per suite per run, never per test.
///
/// Held in memory only: after a restart a suite has no head, and each of its tests is looked
/// up by its asks (exact, a lookup per test where the head's no-op is one read; said in the
/// design's table).
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
        Ok(match held {
            Some(held) if held.binary == binary => {
                // clone: a reference count, once per suite per run
                Taken { now: held.image.clone(), before: None, head: held.head, held: Some(held) }
            }
            Some(held) => Taken {
                now: Arc::new(image()?),
                // clone: a reference count, once per suite per run
                before: Some(held.image.clone()),
                head: held.head,
                held: Some(held),
            },
            None => Taken { now: Arc::new(image()?), before: None, head: None, held: None },
        })
    }

    /// Keeps `held` for the next run of the suite `given`.
    pub(crate) fn give(&self, given: ContentId, held: Held) {
        self.0.lock().insert(given, held);
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
