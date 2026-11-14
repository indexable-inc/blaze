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

#![deny(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each module; inert under rustc.
#![register_tool(blaze)]

pub mod driver;
pub mod exec;
pub mod front;
pub mod kind;
pub mod machine;
pub mod memo;
pub mod outside;
pub mod pool;
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
