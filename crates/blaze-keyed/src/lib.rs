//! The fresh core's vocabulary (docs/v2/plan/design/target.md): what every builder of the core
//! writes against.
//!
//! - [`kind`]: the record kinds as declarations. One trait, [`kind::Kind`], says per kind its key
//!   and how the key is computed from text before the work, its root layout, its grain, its
//!   trace and where it is kept. The driver, the store and the memo are generic over it.
//! - [`record`]: the head every record starts with, the roots, the fact id layout and its
//!   routing, and read-addressing: a record of a traced kind is filed at the address of its key
//!   and the answers its work saw (docs/v2/plan/design/read-addressed.md).
//! - [`store`]: the store's interface (segments, generations, the `Source` and `Verifier`
//!   seams of the shared cache).
//! - [`exec`]: the executor, the driver's second static policy: an action is (kind, key,
//!   inputs) and its task reads only those inputs, so it runs locally or remotely alike.
//! - [`pool`]: the worker pool's interface.
//! - [`machine`]: the machine descriptor everything is sized by.
//! - [`memo`]: the session memo, a static policy: [`memo::RecordsOnly`], [`memo::Resident`] or
//!   [`memo::Audit`].
//! - [`proof`]: proofs a check makes and code that needs the checked thing takes (decision 18):
//!   [`proof::Holds`], [`proof::Certified`], the snapshot brand, the check counters.
//! - [`outside`]: the two-level form of every outside kind (shapes at slots, values at
//!   addresses) and its lookup, settle and filing machines.
//! - [`driver`]: the sans-IO state machine of one request.
//! - [`front`]: the seam the driver's host calls the front end through: key a unit, write a
//!   surface or a body record; and [`front::Universe::answer`], the one lookup every side
//!   answers a fact by.
//!
//! Nothing here does IO, and nothing here depends on the daemon, the query engine or the old
//! store.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each module; inert under rustc.
#![register_tool(blaze)]


extern crate alloc;
pub mod driver;
pub mod exec;
pub mod front;
pub mod kind;
pub mod machine;
pub mod memo;
pub mod outside;
pub mod pool;
pub mod program;
pub mod proof;
pub mod record;
pub mod store;

#[cfg(test)]
mod tests;

/// A bug a control plants, for the oracle of the simulation to catch (rule 43). Each names
/// the place it is planted in; tests.rs has one control per plant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// [`kind::UnitManifest`]'s key leaves out the late inputs (`env!` and friends).
    KeyLeavesOutLateInputs,
    /// The simulation's tasks drop the last fact they asked from the reads they write.
    ReadDropped,
    /// The simulation's tasks leave out of their reads every fact they asked that was absent.
    AbsenceDropped,
    /// The driver, given an old entry, reads the record at the old address without answering
    /// its shape now: a shared-cache hit that does not replay the reads.
    CacheHitNoReplay,
    /// The simulation's body task asks a fact another body makes without writing it in its
    /// reads.
    MadeNotAsked,
    /// The driver's chunk dictionary leaves out its last fact: a reader the postings miss.
    DictionaryMissesFact,
    /// The driver starts a reader before the maker it waits for settles, and never checks it
    /// against the made table after.
    MakerAfterReader,
    /// The driver starts more tasks than it has workers.
    OverCommitsPool,
    /// The simulation's tasks are not a function of their key and reads: a run in any request
    /// but the first writes the count of earlier requests into its payload, so the first files
    /// what a fresh build files and a later run makes other bytes at the same address.
    TwoValuesUnderAddress,
    /// The driver files a unit's chunks, root and mark at once, not each after the last.
    ManifestBeforeRecords,
    /// [`memo::Resident`] answers a mark by the arena, not by the key: a resident entry that
    /// survives a change of its key's input.
    StaleResident,
    /// The driver, after a unit's mark held, still reads the surface it names: a check after a
    /// proof.
    LooksUpUnderMark,
    /// The simulation's host hands unit 1 unit 0's late table: a unit answered another's
    /// `env!` value under the one id both read.
    LateOfAnotherUnit,
    /// The simulation's body task makes a row of its unit's surface besides its made facts: a
    /// fact the driver, were it to drop it, would leave its readers answered `absent`.
    BodyMakesSurfaceRow,
    /// The driver keeps the bodies of a unit whose bodies are not asked: a body of a unit not
    /// asked is run.
    RunsUnaskedBodies,
    /// The driver demands no maker: a reader of a fact a unit not asked makes is answered from
    /// that unit's empty made table, `absent`.
    DemandAnsweredAbsent,
    /// The driver keeps a unit's last made rows whose maker the unit no longer checks: the rows
    /// of a body that is now only a demanded maker are filed in its unit's manifest.
    DemandedRowsFiled,
    /// The driver holds no run: an item whose lookups missed runs at once, while another item
    /// of its unit is still looking up, so a reader with no shape runs before a maker found by
    /// lookup settles, and whether it runs twice depends on the order the store answers in (a
    /// resident memo answers at once).
    RunBeforeLookups,
    /// The driver starts the held runs whenever nothing is ready to start, effects still out:
    /// a cold run starts while another unit's demand is unsettled, or an item of its own unit
    /// waits for a demanded maker, so it reads `absent` or the maker's value by answer order.
    ColdRunBeforeQuiet,
    /// The driver never starts a wave: a request whose runs are held stops making progress, which
    /// the driver's stall bound must end as [`driver::Outcome::Stalled`], never a hang.
    WaveNeverStarts,
    /// A kept plan stands without its reads answered again: a plan found after a layout it read
    /// moved (command.md, "Checks").
    PlanKeptAfterLayoutMoved,
    /// The simulation's walker keys code by its instance, not by its plan's description: code
    /// found after a body it took in moved (command.md, "Checks").
    CodeKeyLeavesOutBody,
    /// The driver asks a body of every keyed item, not only those its unit's surface names: a
    /// silent item (text the def map does not reach) is run.
    AsksUnnamedBodies,
    /// The link step is given the rows that came and went, not those whose code moved under the
    /// same instance.
    LinkMissesMovedRow,
    /// A plan's body of a unit not asked is answered from the kept program, its maker never
    /// demanded: a body on demand not run when its text moved.
    DemandedBodyNotRerun,
    /// A MIR-alone body ([`kind::Steps::Mir`]) is keyed by its item alone, not by the key that
    /// holds its text: a plan is served a body record made before its text moved.
    MirKeyLeavesOutText,
    /// A kept program's reads of a unit's bodies stand whenever the unit is settled by its mark,
    /// whichever unit manifest they were answered against: after a request killed between its
    /// units' manifests and its kept manifest, a body edit is never seen.
    KeptRowsTrustUnitMarks,
    /// A kept manifest is filed again whenever a unit manifest it names is not a unit's now,
    /// though no row moved: an edit no plan reads writes the program's whole manifest.
    KeptRefiledForUnits,
    /// A program's path is placed as soon as its image is made, before the kept manifest naming
    /// it is durable: a kill between leaves the path naming an image no answer certified.
    PlacedBeforeKept,
    /// A certified answer places no path: one a killed request left unplaced stays so.
    CertifiedNotPlaced,
    /// The simulation's host files a program's image under the digest of its bytes, not under
    /// the kept manifest's key it was given: the O(1) name is not honoured.
    ImageNamedByContent,
    /// A failed task stops the request: the tasks that do not depend on it never run.
    FailureStopsAll,
    /// A kept span table stands without its reads of files answered again: a table found after
    /// its file's bases moved (a comment added above its owner).
    SpanFoundAfterBasesMoved,
    /// No link step runs when only span tables moved and no row did: the image keeps the old
    /// positions.
    SpanOnlyLinkSkipped,
    /// The simulation's walker keys code by its item's line too, as a back end that bakes
    /// positions into code would: a comment edit above an item reruns its code (the floor's
    /// control).
    CodeKeyedByPlace,
}

#[cfg(test)]
thread_local! {
    pub static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

/// Whether `plant` is planted: only ever in this crate's tests.
#[cfg(test)]
pub fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

/// Whether `plant` is planted: only ever in this crate's tests.
#[cfg(not(test))]
#[inline(always)]
pub fn planted(_plant: Plant) -> bool {
    false
}
