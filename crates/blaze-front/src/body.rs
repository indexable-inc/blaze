//! `Task::Body` (docs/v2/plan/design/host.md, "blaze-front"; read-addressed.md): one body's
//! check as one read-addressed `Body` record, what `blaze_keyed::front::Front::body` returns.
//!
//! The rule (host.md, "The rule everything below follows"): the body is checked against
//! records, never against a crate built in memory. Every crate the check reads, the body's own
//! included, was opened from its `Surface` record by [`crate::surface`] before this task runs;
//! this task builds no crate's scopes and has no route that would.
//!
//! The record, written by `record::write_read`, so its head is its address (its key and the
//! answers its shape got):
//!
//! - `reads`, the shape: every fact the check asked, through [`reads::Asks`], with the answer
//!   `Universe::answer` gave, absences included, sorted by id, each once. The body's own text is
//!   no row: it is the key (`BodyInputs.text`).
//! - `facts`: the facts the body makes ([`exports`]): a const's, static's or discriminant's
//!   value, a `const fn`'s MIR as an evaluation reads it, a function's hidden types, what
//!   escapes the body, its own effect summary. Each routes to the body's unit's made table.
//! - `payload`: the body's steps as the item store encodes them (no `Gid`, no position), one
//!   section a step ([`payload`]). Its MIR sections are the body's generic MIR, which a plan's
//!   snapshot reads in place with none of the body's text ([`mir`]); a unit no `check` covers
//!   makes its records with `blaze_keyed::kind::Steps::Mir`, no borrow check.
//!
//! **Reads of another body.** A read of a fact another body makes is an ordinary ask of a made
//! fact: answered from the maker's unit's made table (or, in the body's own unit, from the
//! facts its settled bodies made so far), and in the shape, so a change of it moves this
//! record's address and nothing else does. A read of another body's inference, MIR, lowering,
//! match check, code identity or hashes that is none of the made facts is a typed refusal
//! naming the query ([`front`]'s `OTHER_*`): no fact holds it.
//!
//! The seam to the front end is [`Check`]: [`front::Front`] implements it over the ported front
//! end; the tests implement it over a model universe. [`body`] owns everything else: the shape,
//! the refusals, the made facts' table and the record's bytes.
//!
//! **Floor (rule 67).** A body task's floor is the body's own check plus one mapped lookup per
//! fact it asks (host.md, `blaze-front`, "Floor"). Here: [`Check::check`] is the body's own
//! steps; each ask is one `Universe::answer` (a binary search over the universe's owners and
//! one in the owning table: `log u` over the floor's one search); the shape is sorted once
//! (`r log r` for `r` asks, against the floor's `r` lookups); the record is written once and
//! copied once into aligned memory (one more pass over its bytes than the floor, the cost of
//! `write_read` returning a `Vec`). Nothing here walks the crate or the workspace. What the
//! front end does beneath [`Check::check`] is not yet at that floor: see [`front`]'s module doc.

#![blaze::sans_io]

pub mod exports;
pub mod front;
pub mod mir;
pub mod payload;
pub mod reads;

#[cfg(test)]
mod mir_tests;
#[cfg(test)]
mod tests;

use std::alloc::{Allocator, Global};

use blaze_format::Aligned;
use blaze_keyed::{
    driver::TaskFailed,
    front::Universe,
    kind::{Body, Key, Steps},
    record::{self, StableId},
};

use self::{exports::Exports, payload::Writer, reads::Asks};

// What each of `blaze_keyed::kind::Steps` runs here (its one home says what it is and how it is
// keyed): `Checked`, inference, the built MIR of each owner and the borrow check's verdict;
// `Mir`, inference and the built MIR alone, no borrow check and no lint. A `Mir` record holds no
// verdict, so the item store refuses a borrow check asked of it
// (`hir_ty::body_store::Unserved::UNCHECKED`, rule 74). Its read list is made as a checked
// body's is, from the rows of the inference's and each MIR owner's traces ([`front::Front`]), so
// it is the reads of those steps alone.

/// The front end as the body task sees it: find the body, run its check asking every fact
/// through [`Asks`], and say what the body makes. Implemented over the ported front end by
/// [`front::Front`].
pub trait Check {
    /// The body in this check's universe (`hir_def::DefWithBodyId` for the front end).
    type Def: Copy;

    /// The body the driver's item id `item` names; a typed refusal naming why where there is
    /// none.
    fn def(&self, item: StableId) -> Result<Self::Def, TaskFailed>;

    /// Runs the body's `steps`: each step's answer into `payload`, every fact it reads asked
    /// through `asks`, whose answer is the one the check goes on with. A step that refused a
    /// read fails here, by the name of what it read.
    fn check<A: Allocator>(&self, def: Self::Def, steps: Steps, asks: &mut Asks<'_, A>, payload: &mut Writer) -> Result<(), TaskFailed>;

    /// The facts the body `def` makes, into `out`.
    fn exports(&self, def: Self::Def, out: &mut Exports) -> Result<(), TaskFailed>;
}

/// What a body task with no own table is refused as: a body is checked in its unit's universe.
pub const NO_OWN_TABLE: &str = "a body task: its universe has no table of its own unit (`Universe::own`)";

/// `Front::body` (crates/blaze-keyed/src/front.rs) over `check`: the `Body` record of `item`
/// for `key`, its shape over `universe`, 16-aligned, filed at its address.
pub fn body<C: Check>(check: &C, item: StableId, key: Key<Body>, universe: Universe<'_>) -> Result<Aligned, TaskFailed> {
    body_with(check, item, key, universe, Steps::Checked)
}

/// [`body`] running `steps`: `key` must be the one `blaze_keyed::kind::Steps::key` makes of
/// `steps`, the item's Checked key and `item`, so the two records of one body never share a key
/// (rule 21). The driver makes it; this task takes it as given.
pub fn body_with<C: Check>(
    check: &C,
    item: StableId,
    key: Key<Body>,
    universe: Universe<'_>,
    steps: Steps,
) -> Result<Aligned, TaskFailed> {
    let owner = universe.own().ok_or(TaskFailed::Refused { what: NO_OWN_TABLE })?.owner;
    let def = check.def(item)?;
    let mut payload = Writer::new();
    let mut asks = Asks::new(universe, Global);
    check.check(def, steps, &mut asks, &mut payload)?;
    let shape = asks.finish();
    let mut exports = Exports::new();
    check.exports(def, &mut exports)?;
    let made = exports.finish(owner, Global)?;
    let payload = payload.finish(Global);
    let (_, bytes) = record::write_read::<Body>(key, &shape, &made, &payload);
    Ok(Aligned::copy_from(&bytes))
}
