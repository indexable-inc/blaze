//! A test binary as blaze-testsel selects its tests (docs/v2/plan/lanes/fastcompile/select.md):
//! its execution table, made beside its link and kept with its record ([`table`]), the
//! [`Image`] a selector reads it as ([`image`]), what a test executed as a [`Record`]
//! ([`record`]), and the daemon's one [`Selector`] per suite ([`Selectors`]).


use blaze_codegen::{ExecTable, InstanceKey, driver::Built};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_testsel::{Bound, FnId, Image, Record, Selector, Version};
use blaze_usercode::test::Executed;
use hir_ty_codegen::v2::RootTest;
use parking_lot::Mutex;

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
    ExecTable::new(built, spans, slots, slot_of, beside, tests).map_err(|why| format!("the execution table of `{binary}`: {why}"))
}

/// `table` as a selector reads it. A slot an incremental link emptied ([`ExecTable::EMPTY`]) is
/// given as it is: a slot is never dropped, as a record names slots by place.
pub(crate) fn image(binary: &str, table: &ExecTable) -> Result<Image, String> {
    // alloc: once per test binary relinked, the selector's copy of its functions
    let fns = table.fns.iter().map(|(id, bound)| Version { id: FnId(*id), bound: Bound(*bound) }).collect();
    // clone: once per test binary relinked, the selector's copy of its slots
    Image::new(fns, table.slot_fn.clone(), table.beside)
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

/// The daemon's selectors, one per suite (its given) for the daemon's life: each test run
/// takes its suite's ([`Selectors::take`]) and gives it back when it ends having kept every
/// result ([`Selectors::give`]). A run that did not drops it, and the next makes it again from
/// the store, so a selector is always the only writer of its suite's history. The lock is
/// taken twice per suite per run, never per test.
#[derive(Default)]
pub(crate) struct Selectors(Mutex<HashMap<ContentId, Held>>);

/// A selector, with the binary its image is of.
struct Held {
    binary: ContentId,
    selector: Selector,
}

impl Selectors {
    /// The selector of the suite `given` for its binary `binary`: the one kept, moved to the
    /// binary by `image` when it was relinked since, else a new one.
    pub(crate) fn take(
        &self,
        given: ContentId,
        binary: ContentId,
        image: impl FnOnce() -> Result<Image, String>,
    ) -> Result<Selector, String> {
        // An insert or a remove that panicked left the map as it was: its value stands.
        let held = self.0.lock().remove(&given);
        match held {
            Some(held) if held.binary == binary => Ok(held.selector),
            Some(mut held) => {
                held.selector.advance(image()?);
                Ok(held.selector)
            }
            None => Ok(Selector::new(given, image()?)),
        }
    }

    /// Keeps `selector`, whose image is of `binary`, for the next run of its suite.
    pub(crate) fn give(&self, binary: ContentId, selector: Selector) {
        let given = selector.given();
        self.0.lock().insert(given, Held { binary, selector });
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.0.lock().len()
    }
}
