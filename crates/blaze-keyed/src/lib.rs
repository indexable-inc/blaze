//! The fresh core's vocabulary (docs/v2/plan/design/target.md): what every builder of the core
//! writes against.
//!
//! - [`kind`]: the record kinds as declarations. One trait, [`kind::Kind`], says per kind its key
//!   and how the key is computed from text before the work, its root layout, its grain, its
//!   trace and where it is kept. The driver, the store and the memo are generic over it.
//! - [`text`]: text-closure keys: an item's own text plus what each name it mentions can resolve
//!   to, by content, as a Merkle closure over strongly connected components.
//! - [`record`]: the head every record starts with, the roots, and the read list a record of a
//!   traced kind carries.
//! - [`store`]: the store's interface (segments, generations, the `Source` and `Verifier`
//!   seams of the shared cache).
//! - [`exec`]: the executor, the driver's second static policy: an action is (kind, key,
//!   inputs) and its task reads only those inputs, so it runs locally or remotely alike.
//! - [`pool`]: the worker pool's interface.
//! - [`machine`]: the machine descriptor everything is sized by.
//! - [`memo`]: the session memo, a static policy: [`memo::RecordsOnly`] or [`memo::Resident`].
//! - [`proof`]: proofs a check makes and code that needs the checked thing takes (decision 18):
//!   [`proof::Holds`], [`proof::Certified`], the snapshot brand, the check counters.
//! - [`outside`]: the two-level form of every outside kind (shapes at slots, values at
//!   addresses) and its lookup, settle and filing machines.
//! - [`driver`]: the sans-IO state machine of one request.
//!
//! Nothing here does IO, and nothing here depends on the daemon, the query engine or the old
//! store.

#![deny(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each module; inert under rustc.
#![register_tool(blaze)]

pub mod driver;
pub mod exec;
pub mod kind;
pub mod machine;
pub mod memo;
pub mod outside;
pub mod pool;
pub mod proof;
pub mod record;
pub mod store;
pub mod text;

#[cfg(test)]
mod tests;

/// A bug a control plants, for the oracle of the simulation to catch (rule 43). Each names
/// the place it is planted in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// [`kind::Body`]'s key leaves out the first name the body mentions: a type missing from
    /// the closure.
    KeyLeavesOutNames,
    /// [`kind::UnitManifest`]'s key leaves out the late inputs (`env!` and friends).
    KeyLeavesOutLateInputs,
    /// The simulation's body task drops the last binding from the read list it writes.
    ReadDropped,
    /// The driver starts more tasks than it has workers.
    OverCommitsPool,
    /// The driver files a unit's manifest before the last record it names is written.
    ManifestBeforeRecords,
    /// [`memo::Resident`] answers a unit's manifest by the unit, not by the key: a resident entry
    /// that survives a change of its key's input.
    StaleResident,
    /// The driver, after a record's key hit, still fetches and verifies the record the last
    /// manifest named for the item: a check after a proof.
    ChecksAfterHit,
    /// The driver, after a unit's manifest hit, still looks up each record it names.
    LooksUpUnderHeldManifest,
    /// The simulation keys a unit's manifest by its dependencies' manifests, not their
    /// surfaces: a dependency's private body edit makes every dependent be examined.
    UnitKeyedByDepManifests,
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
