//! Impl tables and trait candidate sets maintained on the write side (design
//! `docs/v2/plan/design/dbsp.md` §7, §8; notes `docs/v2/plan/lanes/dbsp/impls.md`).
//!
//! After name resolution's maintainer (order `100`) has published a write's scopes, this one
//! (`200`) reads its delta ([`NameresWrite`]): impls and derives added or removed, traits declared
//! or gone, bindings changed and defs re-read. It recomputes the key ([`key::ImplKey`]: trait
//! and simplified self type, as `TraitImpls`/`InherentImpls` index) of each impl the write
//! could have changed (its read set met the delta), moves the rows whose key changed in the
//! arrangements, and sets the query inputs ([`publish`]) of each `(crate, trait)`,
//! `(crate, head)` and `(crate, name)` slice whose rows changed. Their `ContentId`s are queries
//! over the rows, through the impl and def identities lane item-hash installs.
//!
//! The impls of body blocks stay in per-module pulled tables; the maintainer reads again the
//! body parts of the modules a write may have changed the bodies of, and each slice lists the
//! modules whose bodies hold impls of its key. It then hands the next maintainer (order `300`,
//! lane items' hash columns, design §8b) what changed: [`LastWrite`].
//!
//! `TraitImpls::for_crate_and_trait`, `InherentImpls::in_crate_for_self_ty` and the trait
//! candidates read those inputs ([`read`]), and no reader goes over a crate's modules. A crate
//! the frozen image serves is never maintained: its readers read the image.

mod head;
mod key;
mod publish;
pub(crate) mod read;
mod state;
#[cfg(test)]
mod tests;

use base_db::{Crate, Db, Maintainer, Reads, Written};
use blaze_mem::Tag;
use hir_def::{ImplId, TraitId, nameres::delta::LastWrite as NameresWrite};
use hir_expand::name::Name;

pub use self::{
    head::{Head, HeadDef},
    publish::{
        CANDIDATE_SET_DOMAIN, CandidateSlice, IMPL_SET_DOMAIN, ImplIdentities, ImplSlices,
        InherentSlice, SliceDirectory, TraitImplsSlice, candidate_set_hash, impl_identity,
        impl_set_hash, install_identities,
    },
    read::{blanket_hash, impl_row_hash},
    state::State,
};
/// Every name with a candidate slice in `krate`, emptied ones too: reads the crate's directory
/// of names, which gains a name once and never loses one. `krate` must be maintained (not
/// frozen).
pub fn candidate_names(db: &Db, krate: Crate) -> impl Iterator<Item = Name> + '_ {
    publish::directory(db, krate).names(db).keys().copied()
}

/// The traits declared in `krate`'s modules with an associated item named `name`, sorted by
/// `TraitId`: reads that name's slice alone. `krate` must be maintained (not frozen).
pub fn candidate_traits(db: &Db, krate: Crate, name: Name) -> &[TraitId] {
    match publish::candidate_slice(db, krate, name) {
        Some(slice) => &slice.rows(db).traits,
        None => &[],
    }
}

/// What the last write changed of each maintained crate's impl keys and trait candidates, in
/// dependency order: what the maintainers after this one read (`db.with_state`). Cleared at the
/// start of each write.
#[derive(Default)]
pub struct LastWrite {
    pub crates: Vec<ImplsDelta>,
}

/// One crate's share of [`LastWrite`]; a crate the write left alone has none.
#[derive(Debug, PartialEq, Eq)]
pub struct ImplsDelta {
    pub krate: Crate,
    /// The impls whose `ImplKey` (trait and simplified self type) changed, or that were added
    /// or removed, sorted. The impls of body blocks have no key and are not here.
    pub keys_changed: Vec<ImplId>,
    /// The traits whose associated item names changed (declared, gone or re-read), sorted.
    pub traits_changed: Vec<TraitId>,
}

/// The impls maintainer's reads, in two steps across every crate at once: first what each
/// crate's run reads of the crate, its impls, derives and traits ([`state::warm`]), then the
/// body parts of the modules it reads, which the crates' def maps the first step read name
/// ([`state::warm_bodies`]).
fn read_step(db: &Db, written: &Written<'_>, reads: &mut Reads) {
    let step = reads.step();
    let warm = db.lend_state::<NameresWrite, _>(|last| {
        db.lend_state::<State, _>(|state| match step {
            0 => state::warm(db, state, last, written),
            _ => state::warm_bodies(db, state, last, written),
        })
    });
    reads.each(Tag::Infer, warm, state::warm_one);
    if step == 0 {
        reads.again();
    }
}

/// The impls maintainer's apply, after the second step's reads: one write's delta applied to
/// every crate not frozen, in dependency order, over the memos the reads made.
fn apply_step(db: &mut Db, written: &Written<'_>, reads: &Reads) {
    if reads.step() == 0 {
        return;
    }
    let mut out = db.with_state::<LastWrite, _>(|_, it| std::mem::take(&mut it.crates));
    out.clear();
    db.with_state::<NameresWrite, _>(|db, last| {
        db.with_state::<State, _>(|db, state| state::run(db, state, last, written, &mut out))
    });
    db.with_state::<LastWrite, _>(|_, it| it.crates = out);
}

inventory::submit! { Maintainer { order: 200, name: state::NAME, reads: read_step, apply: apply_step } }
