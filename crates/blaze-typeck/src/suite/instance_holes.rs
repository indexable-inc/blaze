//! A one-shot instance question (instance.rs `select_impl`, `normalize_projection`,
//! `resolve_instance`; drop_impl.rs `adt_drop_impl`) read through a hole answers nothing.
//!
//! `Reads` turns a world's `Missing` into a hole whose stand-in is an empty answer, so a body's
//! run goes on and names every need at once (world.rs `Hole`); a body's run that met one makes
//! no result (lib.rs `check`). The instance questions had no such rule: the stand-in decided
//! them. At tempo's `CharPredicateSearcher<'a, F>` read in a generic body, the impls of
//! `MultiCharEq` at `IsWhitespace` were never asked while checking, the empty stand-in made
//! `impl<C: MultiCharEq> Pattern for MultiCharEqPattern<C>` not hold, and the GAT
//! `<MultiCharEqPattern<IsWhitespace> as Pattern>::Searcher<'a>` was refused as a mismatch
//! (census classes 37 and 35). Each case holds back one question and wants it named; its
//! control answers the question and wants the plain answer.

use blaze_own::{Alloc, Vec};
use std::alloc::Global;

use crate::ids::{ItemKey, List, Name};
use crate::instance::ResolveError;
use crate::portable::{PNode, PortableTys};
use crate::toy::{FieldTy, ToyStruct, ToyWorld, struct_key};
use crate::world::{Question, Shape};

/// A trait no record of the toy world names: only its impls are asked.
const TRAIT: ItemKey = ItemKey(0x7_0000);

fn world(held: Option<Question>) -> ToyWorld {
    let mut fields = Vec::new_in(Global);
    fields.push((Name(1), FieldTy::I32));
    let mut structs = Vec::new_in(Global);
    structs.push(ToyStruct { fields });
    let mut w = match ToyWorld::new(&structs, &[FieldTy::Struct(0)], FieldTy::I32) {
        Ok(w) => w,
        Err(e) => panic!("the toy world could not be built: {e}"),
    };
    w.missing.extend(held);
    w
}

fn select(w: &ToyWorld) -> Result<Option<ItemKey>, ResolveError> {
    let mut ground = PortableTys::new_in(Alloc::global());
    let s0 = ground.intern(PNode::Adt(struct_key(0), List::EMPTY)).expect("the ground table has room");
    crate::instance::select_impl(w, TRAIT, &[s0], &mut ground, Alloc::global()).map(|o| o.map(|(imp, _)| imp))
}

/// `S0: Trait` with the impls of `Trait` at `S0` held back: `Missing` naming that question.
/// Without `instance.rs` `unholed` it is `Ok(None)`, "no impl holds", read from the stand-in.
/// Prints `instance holes: select Err(Missing(ImplsOf(..)))`.
#[test]
fn a_selection_through_an_unmade_bucket_names_the_bucket() {
    let bucket = Question::ImplsOf(TRAIT, Shape::Adt(struct_key(0)));
    let got = select(&world(Some(bucket)));
    std::println!("instance holes: select {got:?}");
    assert_eq!(got, Err(ResolveError::Missing(bucket)));
    // Control: the world answers the bucket (no impl in it), so no impl holds.
    assert_eq!(select(&world(None)), Ok(None));
}

/// `S0`'s `Drop` impl with the bucket held back: `Missing`, never "no `Drop` impl" (which
/// would drop an `S0` by its fields alone). Prints `instance holes: drop Err(Missing(..))`.
#[test]
fn a_drop_impl_through_an_unmade_bucket_names_the_bucket() {
    let bucket = Question::ImplsOf(TRAIT, Shape::Adt(struct_key(0)));
    let got = crate::drop_impl::adt_drop_impl(&world(Some(bucket)), TRAIT, struct_key(0), Alloc::global());
    std::println!("instance holes: drop {got:?}");
    assert_eq!(got, Err(ResolveError::Missing(bucket)));
    // Control: the bucket answered, empty: no `Drop` impl.
    assert_eq!(crate::drop_impl::adt_drop_impl(&world(None), TRAIT, struct_key(0), Alloc::global()), Ok(None));
}
