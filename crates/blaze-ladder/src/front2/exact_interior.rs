//! Each async block's EXACT interior, made once where its owner's clean check is filed (front2
//! opaque_auto.rs `file`), so the row every other body reads (`World::coroutine`, through
//! `Store::hidden`) holds what the state holds, never the type check's superset.
//!
//! The rule: the Reference, "Async blocks": the future is "roughly equivalent to an enum with
//! one variant per await point, where each variant stores the data needed to resume from its
//! corresponding point"; "Auto traits": `Send` holds when it holds for all it stores. So
//! `async fn worker(..) { tick().await; println!("{}", format!(..)) }` is `Send`: the
//! `fmt::Arguments` of the `format!` (core/src/fmt/mod.rs: `impl !Send for Arguments<'_> {}`) is
//! a temporary of a statement AFTER the `.await` and is never stored. The type check's interior
//! (blaze-typeck rules/coroutine.rs) is every type the body names: a superset, whose `No` a
//! caller took as the answer (std census 3, class C: `Box::pin(worker(..))` refused as
//! `Pin<Box<dyn Future + Send>>`).
//!
//! ONE HOME (rule 30): which locals a state stores is blaze-flow suspend.rs `saved` (D-A1: maybe
//! storage-live at a `Suspend` and live by use or maybe borrowed there; a temporary's storage
//! ends at the end of its enclosing statement, blaze-flow-lower lib.rs `CloseTemps`, so one made
//! before an `.await` in the `.await`'s own statement is counted). blaze-flow-lower
//! `interior_parts` reads it for the interior, and `state_recipe` reads the same rule over the
//! same lowering for the layout (run/lay_states.rs). The block is lowered as its WITNESS
//! (`lower_async_witness` over [`NestedFacts`], the facts gathered by borrows.rs `gather_facts`,
//! their one home): the borrow check's statements, loans and saved bits, with no region related
//! (one_call.rs `Erased`), so only a form or layout refusal leaves the row a superset.
//!
//! KEYS (rule 21): the row is made in this command from the body's own check record and the
//! facts `gather_facts` reads of the records (ADT facts, callee heads); `Store::hidden` lives one
//! command. A body that reads the row records `Question::Coroutine` with the answer's content
//! (`CoroutineFacts: ContentHash`: its output and interior, by the table's content), so its read
//! list covers the exact interior it was checked against.
//!
//! A BLOCK WHOSE FLOW IS REFUSED (an `.await` on a future the record does not wholly know,
//! `Form::AwaitOperand`, and the other refusals by form) keeps its refusal here; its row keeps
//! the check's superset (opaque_auto.rs `file_row`), whose `Yes` is sound and whose `No` an
//! unsizing site takes as the interior gap (blaze-typeck rules/coerce_interior.rs). Owed: the
//! row says which it holds, so a plain auto goal resting on a superset `No` is the gap too.

use blaze_flow_lower::{InteriorPart, Refusal, lower_async_witness};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;

use super::body::Body;
use super::borrows::{BorrowLangs, gather_facts};
use super::borrows_nested::NestedFacts;
use super::check::Records;
use super::source::NamesSource;

/// One async block's exact interior: the elements of its tuple, types of the owner's check
/// record (`Checked::types`), sorted by record id; or why the block's flow was refused.
pub enum BlockInterior<'a> {
    Exact(AVec<'a, InteriorPart>),
    Refused(Refusal),
}

/// The exact interiors of one body's async blocks, by the block's expression id, in expression
/// order.
pub struct Exacts<'a> {
    blocks: AVec<'a, (tc::ExprId, BlockInterior<'a>)>,
}

impl<'a> Exacts<'a> {
    /// Block `block`'s interior; `None` when `block` is no async block of the body.
    pub fn of(&self, block: tc::ExprId) -> Option<&BlockInterior<'a>> {
        self.blocks.iter().find(|b| b.0 == block).map(|b| &b.1)
    }
}

/// The exact interior of every async block of `body`, checked clean as `done`. `None` when the
/// body has no async block: no facts are gathered and nothing is lowered (rule 77).
pub fn exact_interiors<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    done: &tc::Checked<'_>,
    alloc: Alloc<'a>,
) -> Result<Option<Exacts<'a>>, tc::CheckError> {
    let exprs = &body.typeck.exprs;
    if !exprs.iter().any(|e| matches!(e, tc::Expr::AsyncBlock { .. })) {
        return Ok(None);
    }
    let langs = BorrowLangs::of(r.index);
    let facts = gather_facts(r, body.typeck.owner, done, langs, alloc);
    // Every fn's signature regions are its head record's, read in place (D148).
    let reader = r.store.regions.reader(r.store.tys.alloc());
    let nested = NestedFacts::new(facts.read_by(&reader, r.store.tys.view()));
    let tys = body.tys.view(&r.store.tys)?;
    let view = body.typeck.view(tys);
    let mut blocks = AVec::new_in(alloc);
    for (i, e) in exprs.iter().enumerate() {
        if !matches!(e, tc::Expr::AsyncBlock { .. }) {
            continue;
        }
        let block = tc::ExprId(i as u32);
        // The WITNESS lowering (one_call.rs `Erased`): what the state stores needs no region, so
        // a region refusal of the borrow lowering (tempo's 45 and 65 rows) no longer leaves the
        // row a superset; only a form or layout refusal does (typeck.md "Witness records", W1).
        let got = match lower_async_witness(&view, done, block, &nested, alloc).and_then(|w| w.interior_parts(done, block, alloc)) {
            Ok(parts) => BlockInterior::Exact(parts),
            Err(why) => BlockInterior::Refused(why),
        };
        blocks.push((block, got));
    }
    Ok(Some(Exacts { blocks }))
}
