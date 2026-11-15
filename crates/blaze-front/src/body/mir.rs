//! A body's generic MIR served from its `Body` record in another snapshot (command.md, "Code";
//! the common brief's decision 3): what a plan's task reads of a dependency's body, with none
//! of the dependency's text.
//!
//! **What the record holds.** A body task already writes, as its payload ([`super::payload`]),
//! one section per step: the inference, the built MIR of each owner the body lowers (the body,
//! each closure and `async` coroutine, at its place: `hir_ty::body_store`'s MIR schema), and
//! the borrow check's verdict when it ran (`blaze_keyed::kind::Steps::Checked`). Each MIR section is the item
//! store's MIR answer (crates/hir-ty/src/body_store/mir.rs): `mir::persist`'s buffer, its items
//! by key (`BodyFacts::def_key`: a crate-relative item key, a closure's or opaque's relative to
//! the body), its types by content (the answer's type table), its positions as the body's own
//! node ids (item-relative, rule 71), its location literals as slots, and the counts of the
//! body's store, so a reader lowers nothing. No `Gid`, address, path or time is in it (rule 25).
//! The MIR is generic: built before any instance, with promoted constants in the body
//! (`Operand::Constant` of an unevaluated promoted) as built MIR has them.
//!
//! What `check` pays for it: nothing it did not pay already. The MIR is built for the borrow
//! check, and the trace that records the body's reads encodes the answer anyway; the cost is
//! its bytes in each body record.
//!
//! **Serving.** [`Records`] is `hir_ty::body_store::GivenBodies` over the body records the
//! driver gives a plan's snapshot, installed with `hir_ty::body_store::install_given`. A query
//! of a body's MIR (`hir_ty::providers::mir_at(db, owner, MirStage::Built)`) finds the body's
//! record by its item key (`blaze_item_hash::values::body_key`, the fact subject the driver
//! names a body by: its maker row's and `record::mir_id`'s), the section of the owner's
//! schema, and decodes it in place in that database; it lists the body's MIR fact
//! (`blaze_keyed::record::mir_id`, digest the record's result digest) as what it read, which is
//! the fact a plan's shape holds of the body. With no record the query refuses
//! (`Unserved::NO_RECORD`) naming the item, which [`failed`] makes the driver's
//! `TaskFailed::NoBody`: the driver finds or makes that body's record and runs the plan again.
//!
//! The invariant: a record given here is one the driver checked under its key
//! (`record::check_read`), and its digest is that record's `record::result_digest`.
//!
//! **Floor (an estimate, rule 67).** One lookup and the bytes mapped: per owner asked, one
//! item key (memoized), a binary search over the given records (`log n`), a walk of the
//! record's section heads up to the one asked (a handful), and the section read in place; the
//! items it names are made once per snapshot (each a memoized `def_of` of its key). Here: the
//! record's root is opened on every ask (one bounds check per slice), and its payload is found
//! again for each owner of one body, a constant factor of one or two over the floor. Above it:
//! [`given`] copies each record once into a holder the store keeps, because the plan task is
//! handed borrowed bytes and the store outlives no borrow (its registry is `'static`): one
//! pass over at most `BODIES_MOST` records per plan run, against the floor's none.

#![blaze::sans_io]

use std::{
    alloc::{Allocator, Global},
    ops::Range,
    sync::Arc,
};

use base_db::Db;
use blaze_format::{Aligned, SchemaHash};
use blaze_ids::ContentId;
use blaze_item_hash::values;
use blaze_keyed::{
    driver::TaskFailed,
    front::GivenBody,
    record::{self, Fact, StableId, Traced},
};
use hir_def::{DefWithBodyId, deps::ImageBuf};
use hir_ty::body_store::{Given, GivenBodies, Step, Unserved};

use super::payload;

/// What a given record that does not open as a `Body` record is refused as.
pub const NOT_A_RECORD: &str = "a given body record that does not open as one (`record::check_read` was not its check)";
/// What a given record whose payload is not one is refused as.
pub const NOT_A_PAYLOAD: &str = "a given body record whose payload is not one of this format";

/// One given body record: the body's item key (its fact subject, `values::body_key`), the
/// record's bytes (16-aligned, held as the store keeps them), and its result digest
/// (`record::result_digest`, what the driver answers the body's MIR fact with).
pub struct GivenRecord {
    pub item: StableId,
    pub held: ImageBuf,
    pub digest: ContentId,
}

/// Body records given to a plan's snapshot: each body's by its item key, sorted by it, each
/// once.
pub struct Records<A: Allocator> {
    records: Vec<GivenRecord, A>,
}

/// What [`Records::new`] refuses.
pub const UNSORTED: &str = "given body records not sorted by their item, each once";

impl<A: Allocator + Send + Sync + 'static> Records<A> {
    /// The records of `records`, sorted by item, each once, else refused by [`UNSORTED`].
    pub fn new(records: Vec<GivenRecord, A>) -> Result<Self, &'static str> {
        if !records.windows(2).all(|pair| pair[0].item < pair[1].item) {
            return Err(UNSORTED);
        }
        Ok(Records { records })
    }
}

/// The records a plan task is given (`blaze_keyed::front::Front::plan`'s `bodies`, sorted by
/// item, each checked by the driver under its key), as the store of a plan's snapshot takes
/// them: `hir_ty::body_store::install_given(db, facts, build, given(bodies, alloc)?)`. The
/// driver's item is the body's fact subject, which is the item key [`Records`] looks a body up
/// by, so it is taken as it is.
///
/// Each record is copied once into an aligned holder of its own: `bodies` are borrowed from the
/// worker for the task, and the store of a snapshot is `'static` (its registry), so it cannot
/// hold the borrow. What removes the copy is the host handing the holder it read the record
/// into (`GivenBody` carrying an `Arc<Aligned>` beside its bytes), cloned here by count.
pub fn given<A: Allocator + Send + Sync + 'static>(bodies: &[GivenBody<'_>], alloc: A) -> Result<Records<A>, &'static str> {
    let mut records = Vec::with_capacity_in(bodies.len(), alloc);
    for body in bodies {
        // dyn: a record held as the store holds it
        let held: ImageBuf = Arc::new_in(Aligned::copy_from(body.record), Global);
        records.push(GivenRecord { item: body.item, held, digest: body.result });
    }
    Records::new(records)
}

/// A plan task's failure for the store's refusal `unserved`: a body with no record given is the
/// driver's `TaskFailed::NoBody` of its item (the subject `record::mir_id` takes), so the driver
/// finds or makes that record and runs the plan again given it; every other refusal, and a body
/// with no item key, fails the plan by the refusal's name.
pub fn failed(unserved: &Unserved) -> TaskFailed {
    match (unserved.kind, unserved.item) {
        (Unserved::NO_RECORD, Some(item)) => TaskFailed::NoBody { item: StableId(item) },
        (kind, _) => TaskFailed::Refused { what: kind },
    }
}

impl<A: Allocator + Send + Sync + 'static> GivenBodies for Records<A> {
    fn answer(&self, db: &Db, def: DefWithBodyId, step: Step, schema: SchemaHash) -> Given {
        let item = values::body_key(db, def);
        let Ok(at) = self.records.binary_search_by(|it| it.item.0.cmp(&item)) else {
            return Given::NoRecord { item: Some(item) };
        };
        let given = &self.records[at];
        let bytes = given.held.bytes();
        let Ok(root) = blaze_format::open::<Traced>(bytes) else {
            return Given::Unreadable { item, why: NOT_A_RECORD };
        };
        let Ok(payload) = root.payload.follow(bytes) else {
            return Given::Unreadable { item, why: NOT_A_RECORD };
        };
        let base = payload.as_ptr().addr() - bytes.as_ptr().addr();
        match payload::find(payload, step as u8, schema.0) {
            Ok(Some(range)) => Given::Answer {
                // clone: a counted reference to the mapped record, held by the asking query's answer
                held: given.held.clone(),
                range: shift(range, base),
                fact: Fact { id: record::mir_id(&StableId(item)), digest: given.digest },
                item,
            },
            Ok(None) => Given::NotInRecord { item },
            Err(_) => Given::Unreadable { item, why: NOT_A_PAYLOAD },
        }
    }
}

fn shift(range: Range<usize>, by: usize) -> Range<usize> {
    range.start + by..range.end + by
}
