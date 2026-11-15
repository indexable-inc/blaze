//! A body record's shape: every fact its check asked, with the answer it got, through the one
//! reader that records it ([`Asks`]).
//!
//! - Every ask is answered by `blaze_keyed::front::Universe::answer` on the tables the task was
//!   handed (its dependencies' surfaces and made tables, its own unit's), and the answer is what
//!   the shape holds. The check reads no fact any other way; a fact it reads past [`Asks`] is a
//!   hole in the shape, which blaze-keyed's `Audit` policy finds by redoing the record (the
//!   test `a_planted_missing_ask_is_caught_by_a_rerun`).
//! - A negative read (a name looked up and not found, an impl sought and absent) is an ask like
//!   any other: its answer is `record::absent(&id)`, so a name that appears moves the answer and
//!   with it the record's address, and a name still absent keeps it.
//! - A fact another body makes (a const's value, an opaque type's hidden type, what escapes a
//!   body, its effects) is an ask like any other too: its id routes to the maker's unit's made
//!   table, and its answer lands in the shape.
//! - The record's own text is no row: it is the record's key (`BodyInputs.text`).
//! - The shape is a set, sorted by id, each once (read-addressed.md, "Shapes are sets"):
//!   one universe answers one id one way, so two asks of one id are one row.
//!
//! Floor: one binary search per ask (the answer) and one sort of the shape; nothing per crate.

#![blaze::sans_io]

use std::alloc::Allocator;

use blaze_ids::ContentId;
use blaze_keyed::{
    driver::TaskFailed,
    front::Universe,
    record::Fact,
};

/// What one row a step of the check read stands for in the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    /// A function of the record itself: the body's own syntax, its own earlier steps (its
    /// inference as its MIR reads it), its own code identity.
    Covered,
    /// A fact, by id: asked.
    Fact(ContentId),
    /// Something no fact stands for, named.
    Refused(&'static str),
    /// A fact refused: the rule that refused it (`what`), the front end's query kind of the
    /// fact and the subject (an item, module or crate key), so the failure names the item and
    /// the kind it is about (`TaskFailed::RefusedFact`), not the rule alone.
    RefusedFact { what: &'static str, kind: u16, subject: ContentId },
}

/// The one reader of facts of a body task: answers each ask and keeps it for the shape.
pub struct Asks<'u, A: Allocator> {
    universe: Universe<'u>,
    shape: Vec<Fact, A>,
}

impl<'u, A: Allocator> Asks<'u, A> {
    /// The shape kept in `alloc`.
    pub fn new(universe: Universe<'u>, alloc: A) -> Asks<'u, A> {
        Asks { universe, shape: Vec::new_in(alloc) }
    }

    /// The answer now of fact `id`, kept for the shape. An id the universe cannot answer fails
    /// the task by the rule it broke (`Unanswerable::failed`).
    pub fn ask(&mut self, id: &ContentId) -> Result<ContentId, TaskFailed> {
        let digest = self.universe.answer(id).map_err(|it| it.failed())?;
        self.shape.push(Fact { id: *id, digest });
        Ok(digest)
    }

    /// Takes `row`: asks a fact, passes what the record covers, fails on a refused read by its
    /// name.
    pub fn offer(&mut self, row: Row) -> Result<(), TaskFailed> {
        match row {
            Row::Covered => Ok(()),
            Row::Fact(id) => self.ask(&id).map(|_| ()),
            Row::Refused(what) => Err(TaskFailed::Refused { what }),
            Row::RefusedFact { what, kind, subject } => {
                Err(TaskFailed::RefusedFact { what, kind, subject })
            }
        }
    }

    /// The shape as the record keeps it: sorted by id, each once.
    pub fn finish(mut self) -> Vec<Fact, A> {
        self.shape.sort_unstable();
        self.shape.dedup_by_key(|fact| fact.id);
        self.shape
    }
}
