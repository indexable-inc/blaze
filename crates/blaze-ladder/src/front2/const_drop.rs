//! The two facts the constant evaluator's `needs_drop` intrinsic reads, answered over front2's
//! records (consts.rs `ConstsWorld`): whether a ground ADT type has a `Drop` impl
//! ([`drop_impl`], `ConstWorld::drop_impl`) and which ADT is core's `ManuallyDrop`
//! ([`manually_drop`], `ConstWorld::manually_drop`). The rule itself has one home, blaze-build
//! drop_facts.rs `needs_drop` (blaze-consteval drop_ask.rs asks it); the `Drop` impl of an ADT
//! has one home too, blaze-typeck drop_impl.rs `adt_drop_impl`, which the run path's builder
//! world asks as well (run.rs `drop_impl`). This file only hands front2's records to them.
//!
//! The rule (the Reference, "Destructors": "If `T: Drop`, calling `<T as core::ops::Drop>::drop`",
//! then each field, element or capture; "Unions": "Union fields are never dropped"; the pinned
//! core, library/core/src/mem/manually_drop.rs: `ManuallyDrop<T>` "inhibit[s] the compiler from
//! automatically calling `T`'s destructor"). Only a struct, enum or union can implement `Drop`
//! (E0120), and an impl covers every instance of its ADT (E0366/E0367), so the impl is found
//! in the ADT's own bucket of the `Drop` trait, whatever crate declares the ADT or the impl.
//!
//! Before tonight's census (tree ffc4e070ba, 14 constants, e.g. crossbeam-epoch's
//! `thread_local! { static HANDLE: LocalHandle = .. }`, whose std expansion asks
//! `needs_drop::<LocalHandle>()` in a constant) `ConstsWorld` answered neither fact (the trait's
//! defaults), and every `needs_drop` reaching an ADT was refused naming the ADT.
//!
//! What the records have not made yet is never answered "no impl": the impls of `Drop` at the
//! ADT's shape, an impl's header, the trait's members are noted as fills with the step's
//! projections' (const_project.rs `ProjectAsk::note`); the step is refused and consts.rs
//! `value_key` makes them and evaluates it again. A question this front end cannot answer is a
//! gap, refused by name the same way (`CheckFail::Gap` naming the constant and the gap).

use core::cell::RefCell;
use std::alloc::Global;
use std::vec::Vec;

use blaze_build::BuildError;
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, PTy, PortableTys};

use super::block_traits::BlockTraits;
use super::const_project::ProjectAsk;
use super::heads::{Store, Want};
use super::index::Index;
use super::keys::{Keys, MemberKind, checker_of};
use super::source::NamesSource;
use super::spell::Spelling;
use super::types::SelfTy;
use super::world_items::{BodyWorld, Fill, Gap};

/// core's `ManuallyDrop` ADT by its `#[lang = "manually_drop"]` (langs.rs `find`, the one
/// home of a lang item's library item): `Some(None)` when the program's crates declare none (a
/// `no_core` program). A library the lookup cannot answer is a gap, noted for the step's
/// caller, and `None` here: the intrinsic is then refused naming the ADT it met, and
/// `value_key` refuses the constant naming the gap.
pub(super) fn manually_drop<'n, S: NamesSource<'n>>(names: S, index: &Index<'n>, project: &ProjectAsk<'_>) -> Option<Option<ItemKey>> {
    match super::langs::find(index, crate::resolve::Paths { names }, tc::LangItem::ManuallyDrop) {
        Ok(found) => Some(found.map(checker_of)),
        Err(why) => {
            project.note(Vec::new_in(Global), one_gap(Gap::Lang(tc::LangItem::ManuallyDrop, why)));
            None
        }
    }
}

/// Whether ground type `ty` (of `ground`) has a `Drop` impl, as `ConstWorld::drop_impl` asks:
/// `Ok(Some(drop))`, the `Drop` trait's `drop` fn (which a call at `[ty]` runs as the impl's,
/// as run.rs answers the builder), or `Ok(None)`. Every answer is `Some`: this world always
/// answers; what it could not answer yet is an `Err` with the fills or gaps noted in `project`
/// (see [`answer`]).
#[allow(clippy::too_many_arguments)]
pub(super) fn drop_impl<'n, S: NamesSource<'n>>(
    names: S,
    keys: &Keys,
    index: &Index<'n>,
    store: &Store<'n>,
    project: &ProjectAsk<'_>,
    ty: PTy,
    ground: &mut PortableTys<'_>,
) -> Option<Result<Option<ItemKey>, BuildError>> {
    let adt = match ground.view().node(ty) {
        Ok(tc::PNode::Adt(k, _)) => k,
        // Only a struct, enum or union can implement `Drop` (E0120): any other type drops only
        // its parts, which drop_facts.rs walks.
        Ok(_) => return Some(Ok(None)),
        Err(_) => return Some(Err(BuildError::Malformed { what: "a type past the ground table" })),
    };
    let drop_lib = match super::langs::find(index, crate::resolve::Paths { names }, tc::LangItem::Drop) {
        Ok(Some(lib)) => lib,
        // A program whose crates declare no `Drop` trait (`no_core`): no type has an impl.
        Ok(None) => return Some(Ok(None)),
        Err(why) => {
            project.note(Vec::new_in(Global), one_gap(Gap::Lang(tc::LangItem::Drop, why)));
            return Some(Err(BuildError::MissingRecord { item: adt }));
        }
    };
    let drop_trait = checker_of(drop_lib);
    let spell = Spelling::fixed();
    // An empty block list, as const_select.rs's: no block's `use` is in the trait's scope.
    let blocks = BlockTraits::module(store.tys.alloc());
    let self_ = SelfTy::None;
    let world = BodyWorld {
        names,
        keys,
        index,
        store,
        buckets: project.buckets(),
        // No path is resolved: the impl is found by the trait and the ADT's shape alone.
        owner: drop_trait,
        module: drop_lib,
        params: &[],
        self_: &self_,
        spell: &spell,
        traits: &[],
        blocks: &blocks,
        path_marks: &[],
        path_ns: &[],
        fills: RefCell::new(Vec::new_in(Global)),
        gaps: RefCell::new(Vec::new_in(Global)),
        // A constant's evaluation is after analysis (rustc's CTFE runs post-analysis): every
        // opaque reached is its hidden type.
        reveal: super::opaque_auto::Reveal::All,
    };
    // The scratch lives in the ground table's allocator: the caller's, handed down (D102).
    let solved = tc::adt_drop_impl(&world, drop_trait, adt, ground.alloc());
    let mut fills = world.fills.into_inner();
    let gaps = world.gaps.into_inner();
    // The trait's `drop` fn is filed with the trait's members (heads.rs `trait_members`); not
    // filed yet, the trait's head and members are made first (as const_qpath.rs makes a
    // trait's constants), and the step asked again.
    let drop_fn = keys.filed_member(drop_trait, MemberKind::Fn, "drop");
    if drop_fn.is_none() && matches!(solved, Ok(Some(_))) {
        fills.push(Fill::Store(Want::Head(drop_lib)));
        fills.push(Fill::Store(Want::Members(drop_lib)));
    }
    let noted = !fills.is_empty() || !gaps.is_empty();
    if noted {
        project.note(fills, gaps);
    }
    Some(answer(adt, solved, noted, drop_fn))
}

/// The answer from what the solver gave (`solved`: the impl, or none), whether the world noted
/// anything to make or a gap (`noted`), and the `Drop` trait's `drop` fn if filed. While
/// anything is noted the solver read only the impls made so far, so even "no impl" is no
/// answer: `MissingRecord` naming the ADT, never a guessed `false` (rule 3). A solver error
/// (two impls, a header it cannot read) is the build's `Select` refusal.
fn answer(adt: ItemKey, solved: Result<Option<ItemKey>, tc::ResolveError>, noted: bool, drop_fn: Option<ItemKey>) -> Result<Option<ItemKey>, BuildError> {
    if noted {
        return Err(BuildError::MissingRecord { item: adt });
    }
    match solved {
        Ok(None) => Ok(None),
        Ok(Some(_)) => drop_fn.map(Some).ok_or(BuildError::Malformed { what: "the `drop` lang trait has no `drop` fn" }),
        Err(e) => Err(BuildError::Select(e)),
    }
}

fn one_gap(gap: Gap) -> Vec<Gap, Global> {
    let mut gaps = Vec::with_capacity_in(1, Global);
    gaps.push(gap);
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The control: the impls of `Drop` at the ADT's shape are not read yet (the world noted a
    /// fill), and the solver, over the impls made so far, found none. The answer must be the
    /// typed refusal naming the ADT, never "no impl": a fix that took the solver's `Ok(None)`
    /// would answer `needs_drop::<LocalHandle>()` `false` for crossbeam-epoch's
    /// `impl Drop for LocalHandle` whenever its crate's impls were not made first.
    /// Prints `test front2::const_drop::tests::unread_impls_are_no_answer ... ok`.
    #[test]
    fn unread_impls_are_no_answer() {
        let adt = ItemKey(0xad7);
        let drop_fn = Some(ItemKey(0xd0));
        assert!(matches!(answer(adt, Ok(None), true, drop_fn), Err(BuildError::MissingRecord { item }) if item == adt), "unread impls are no answer");
        assert!(matches!(answer(adt, Ok(Some(ItemKey(0x1))), true, drop_fn), Err(BuildError::MissingRecord { item }) if item == adt), "noted fills are no answer");
        assert!(matches!(answer(adt, Ok(None), false, drop_fn), Ok(None)), "every impl read, none for the ADT: no `Drop` impl");
        assert!(matches!(answer(adt, Ok(Some(ItemKey(0x1))), false, drop_fn), Ok(Some(f)) if Some(f) == drop_fn), "an impl: the trait's `drop` fn");
    }
}
