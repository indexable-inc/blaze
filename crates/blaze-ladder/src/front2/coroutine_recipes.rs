//! Each async block's STATE RECIPE, made WITH its owner body's check (ASYNC.md P8, option (b),
//! main's rulings of 2026-11-16): a fact the body makes (drive.md, "D114 rulings", "Facts a body
//! makes" (i): what leaks of a hidden type, for code its layout), read by whoever lays the state
//! out (run/lay_states.rs). One home: the recipe is blaze-flow-lower `state_recipe` of the
//! block's own flow (`lower_async`), the flow the borrow check lowers too.
//!
//! A block whose flow is refused keeps its row WITH the refusal: a run that needs that state
//! refuses it by name (run/lay_states.rs), a run that does not is unaffected (as
//! coroutines.rs `CoroutineTable` keeps refused facts).

use std::alloc::Global;
use std::vec::Vec;

use blaze_flow_lower::state_recipe::{StateRecipe, state_recipe};
use blaze_flow_lower::{Refusal, lower_async};
use blaze_own::Alloc;
use blaze_typeck as tc;

use super::body::Body;
use super::borrows::{BorrowLangs, gather_facts};
use super::borrows_nested::NestedFacts;
use super::check::Records;
use super::source::NamesSource;

/// One recipe row: the owner fn, the block's ordinal (blaze-build `coroutines_in`), the recipe
/// or why the block's flow was refused.
pub type RecipeRow = (tc::ItemKey, u32, Result<StateRecipe<Global>, Refusal>);

/// Appends to `rows` the recipe of every async block of `body`, checked as `done`, read over
/// the body's types as its check read them (`body.tys` with the store's table: exact_interior.rs
/// does the same). A body with no async block asks nothing (no facts gathered: rule 77). Refused
/// only when the body's type table does not read, as the check would have refused it.
pub fn recipes_of<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    done: &tc::Checked<'_>,
    rows: &mut Vec<RecipeRow, Global>,
    alloc: Alloc<'_>,
) -> Result<(), tc::CheckError> {
    if !body.typeck.exprs.iter().any(|e| matches!(e, tc::Expr::AsyncBlock { .. })) {
        return Ok(());
    }
    let owner = body.typeck.owner;
    let langs = BorrowLangs::of(r.index);
    let facts = gather_facts(r, owner, done, langs, alloc);
    // Every fn's signature regions are its head record's, read in place (D148).
    let reader = r.store.regions.reader(r.store.tys.alloc());
    let nested = NestedFacts::new(facts.read_by(&reader, r.store.tys.view()));
    let tys = body.tys.view(&r.store.tys)?;
    let view = body.typeck.view(tys);
    for (ordinal, block) in blaze_build::coroutines_in(&view) {
        let recipe = lower_async(&view, done, block, &nested, alloc).and_then(|lowered| state_recipe(&view, &lowered, Global));
        rows.push((owner, ordinal, recipe));
    }
    Ok(())
}

/// `rows` sorted by (owner, ordinal) for [`find`].
pub fn sort(rows: &mut Vec<RecipeRow, Global>) {
    rows.sort_unstable_by_key(|r| (r.0, r.1));
}

/// The row of block `ordinal` of `owner` in sorted `rows`: `None` when the owner has no such
/// block or was not checked.
pub fn find(rows: &[RecipeRow], owner: tc::ItemKey, ordinal: u32) -> Option<Result<&StateRecipe<Global>, Refusal>> {
    let at = rows.binary_search_by_key(&(owner, ordinal), |r| (r.0, r.1)).ok()?;
    Some(rows[at].2.as_ref().map_err(|e| *e))
}
